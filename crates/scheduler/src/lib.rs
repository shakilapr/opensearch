//! Freshness scheduler (plan Stage 3, crawler §39–40, engine §11–12, engine-arch §13).
//!
//! HOT/WARM/COLD classes, the learned change model `P(changed) = 1 - exp(-rate·age)`,
//! priority tiers, a charge-before-dispatch budget ledger, and an activation scheduler with
//! bounded per-origin and total live-fetch fan-out. Pure logic; the crawl loop is wired in
//! `agent0`. Interfaces are frozen in `docs/STAGE3_SPEC.md`.
//!
//! Hard invariants (enforced here, tested in each module):
//! * total in-flight activations never exceed [`SchedulerConfig::total_live_fanout`];
//! * no single origin ever exceeds [`SchedulerConfig::per_origin_fanout`];
//! * the [`BudgetLedger`] is charged *before* an [`Activation`] is returned, and a refused
//!   charge yields no activation (no storm);
//! * `due()` emits HOT before WARM before COLD.

#![forbid(unsafe_code)]

pub mod adaptive;
pub mod budget;
pub mod change;
pub mod class;
pub mod priority;
pub mod scheduler;
pub mod time_sensitive;

pub use adaptive::{next_delay_secs, ChangeHistory};
pub use budget::BudgetLedger;
pub use change::change_probability;
pub use class::{classify_source, SourceClass};
pub use priority::Priority;
pub use scheduler::{
    Activation, ActivationReason, ActivationScheduler, SchedulerConfig, SourceDue,
};
pub use time_sensitive::is_time_sensitive;
