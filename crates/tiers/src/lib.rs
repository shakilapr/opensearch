//! Storage tiers (plan Stage 5): HOT/WARM/COLD placement + move planning.
//! Distinct from the crawler's freshness classes. Interfaces frozen in `docs/specs/STAGE5_SPEC.md`.
//!
//! A tier is a *storage* decision driven by how recently a document was accessed and how large
//! it is — not by how often its source changes. [`classify`] maps one document's metadata to the
//! tier it belongs in; [`plan`] diffs a current assignment against the fresh classification and
//! emits the promote/demote moves needed to converge.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

/// Storage tier, hottest (fastest/cheapest to serve) first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Tier {
    /// Keep in the hot set (fast storage), recently accessed and not oversized.
    Hot,
    /// Warm storage: accessed within the warm horizon but no longer hot.
    Warm,
    /// Cold storage: rarely accessed; eligible for archival.
    Cold,
}

/// Placement policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierPolicy {
    /// A document accessed within this many seconds of `now` is Hot-eligible.
    pub hot_max_age_secs: i64,
    /// A document accessed within this many seconds of `now` is (at least) Warm.
    pub warm_max_age_secs: i64,
    /// A document larger than this many bytes is never Hot (too costly to keep hot).
    pub hot_max_bytes: u64,
}

/// Per-document metadata used for placement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocMeta {
    pub doc_id: String,
    /// Unix seconds of the last access.
    pub last_access: i64,
    /// Unix seconds of the last fetch.
    pub last_fetch: i64,
    /// Stored size in bytes.
    pub bytes: u64,
}

/// Classify one document into a storage tier.
///
/// * age = `now - last_access` (a future `last_access` yields a non-positive age => Hot).
/// * Hot  when `age <= hot_max_age_secs` **and** `bytes <= hot_max_bytes`;
/// * Warm when `age <= warm_max_age_secs`;
/// * Cold otherwise.
///
/// An oversized-but-recent document degrades to Warm rather than Cold.
pub fn classify(meta: &DocMeta, policy: &TierPolicy, now: i64) -> Tier {
    let age = now.saturating_sub(meta.last_access);
    if age <= policy.hot_max_age_secs && meta.bytes <= policy.hot_max_bytes {
        Tier::Hot
    } else if age <= policy.warm_max_age_secs {
        Tier::Warm
    } else {
        Tier::Cold
    }
}

/// A requested storage move: the document id and the target tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TierMove {
    /// Move to a hotter tier.
    Promote(String, Tier),
    /// Move to a colder tier.
    Demote(String, Tier),
}

/// Compute the moves needed to bring `current` (each doc's present tier) in line with the fresh
/// classification. Only changed assignments produce a move; a doc already in its target tier is
/// skipped. Input order is preserved, giving deterministic output.
pub fn plan(current: &[(DocMeta, Tier)], policy: &TierPolicy, now: i64) -> Vec<TierMove> {
    let mut moves = Vec::new();
    for (meta, assigned) in current {
        let target = classify(meta, policy, now);
        if target < *assigned {
            // Lower `Ord` == hotter tier => promotion.
            moves.push(TierMove::Promote(meta.doc_id.clone(), target));
        } else if target > *assigned {
            moves.push(TierMove::Demote(meta.doc_id.clone(), target));
        }
    }
    moves
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> TierPolicy {
        TierPolicy {
            hot_max_age_secs: 3600,
            warm_max_age_secs: 86_400,
            hot_max_bytes: 1_000_000,
        }
    }

    fn doc(id: &str, last_access: i64, bytes: u64) -> DocMeta {
        DocMeta {
            doc_id: id.into(),
            last_access,
            last_fetch: last_access,
            bytes,
        }
    }

    #[test]
    fn classify_keeps_recent_small_docs_hot_and_demotes_old_rare_ones() {
        let p = policy();
        let now = 1_000_000;
        assert_eq!(classify(&doc("fresh", now - 10, 1000), &p, now), Tier::Hot);
        assert_eq!(
            classify(&doc("stale", now - 7200, 1000), &p, now),
            Tier::Warm,
            "older than hot horizon but within warm horizon"
        );
        assert_eq!(
            classify(&doc("archived", now - 200_000, 1000), &p, now),
            Tier::Cold,
            "rarer than the warm horizon"
        );
    }

    #[test]
    fn oversized_document_is_never_hot() {
        let p = policy();
        let now = 1_000_000;
        assert_eq!(
            classify(&doc("big", now - 5, 5_000_000), &p, now),
            Tier::Warm,
            "a large, recently accessed doc degrades from Hot to Warm"
        );
    }

    #[test]
    fn plan_demotes_cold_docs_and_promotes_hot_ones() {
        let p = policy();
        let now = 1_000_000;
        let current = vec![
            // Cold by age but currently assigned Hot -> Demote to Cold.
            (doc("cold-doc", now - 300_000, 1000), Tier::Hot),
            // Hot by age but currently assigned Cold -> Promote to Hot.
            (doc("hot-doc", now - 10, 1000), Tier::Cold),
            // Already correct -> no move.
            (doc("steady", now - 10, 1000), Tier::Hot),
        ];
        let moves = plan(&current, &p, now);
        assert_eq!(
            moves,
            vec![
                TierMove::Demote("cold-doc".into(), Tier::Cold),
                TierMove::Promote("hot-doc".into(), Tier::Hot),
            ],
            "only changed assignments move"
        );
    }

    #[test]
    fn plan_is_empty_when_assignments_are_up_to_date() {
        let p = policy();
        let now = 1_000_000;
        let current = vec![
            (doc("a", now - 10, 1000), Tier::Hot),
            (doc("b", now - 7200, 1000), Tier::Warm),
            (doc("c", now - 300_000, 1000), Tier::Cold),
        ];
        assert!(plan(&current, &p, now).is_empty());
    }

    #[test]
    fn tier_ordering_is_hot_lt_warm_lt_cold() {
        assert!(Tier::Hot < Tier::Warm);
        assert!(Tier::Warm < Tier::Cold);
    }
}
