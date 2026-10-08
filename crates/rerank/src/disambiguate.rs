//! Entity disambiguation that keeps competing interpretations (engine §23 spirit).
//!
//! The job is to *not* collapse "Apple" to one entity at first sight. Each candidate label is
//! scored against the query context (mentions + surrounding text); every candidate scoring at
//! least `keep_ratio * top_score` is kept, so a clear winner yields one hypothesis while an
//! ambiguous mention keeps its rivals for a later disambiguation stage.

use std::collections::HashSet;

use crate::tokenize::tokenize;

/// One candidate interpretation of an ambiguous mention.
#[derive(Debug, Clone, PartialEq)]
pub struct Hypothesis {
    pub entity_id: String,
    pub label: String,
    pub score: f32,
}

/// Keeps competing hypotheses above a fraction of the top score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Disambiguator {
    pub max_hypotheses: usize,
    /// Keep every candidate scoring `>= keep_ratio * top_score`. Default `0.7`.
    pub keep_ratio: f32,
}

/// Default retention fraction. 0.7 keeps two plausible readings when their evidence is within
/// 30% of each other and still drops a clear loser.
pub const DEFAULT_KEEP_RATIO: f32 = 0.7;

impl Disambiguator {
    pub fn new(max_hypotheses: usize) -> Self {
        Self {
            max_hypotheses,
            keep_ratio: DEFAULT_KEEP_RATIO,
        }
    }

    /// Build with an explicit retention ratio (clamped to `[0, 1]`).
    pub fn with_keep_ratio(max_hypotheses: usize, keep_ratio: f32) -> Self {
        Self {
            max_hypotheses,
            keep_ratio: keep_ratio.clamp(0.0, 1.0),
        }
    }

    /// Score each candidate label against `context` (with `mentions` prepended) and keep all
    /// candidates whose score is at least `keep_ratio` of the best score.
    ///
    /// The per-candidate score is the fraction of the label's unique tokens that appear in the
    /// context token set (a label fully present in context scores `1.0`). Results are sorted by
    /// score descending, ties broken by the candidate's position in `candidates`, then capped
    /// at `max_hypotheses`. Empty `candidates` -> empty output.
    pub fn resolve(
        &self,
        mentions: &[String],
        context: &str,
        candidates: &[(String, String)],
    ) -> Vec<Hypothesis> {
        if candidates.is_empty() {
            return Vec::new();
        }

        let mut haystack = String::new();
        for m in mentions {
            haystack.push_str(m);
            haystack.push(' ');
        }
        haystack.push_str(context);
        let ctx: HashSet<String> = tokenize(&haystack).into_iter().collect();

        let mut scored: Vec<(usize, f32)> = candidates
            .iter()
            .enumerate()
            .map(|(i, (_, label))| (i, label_score(label, &ctx)))
            .collect();

        let top = scored.iter().map(|(_, s)| *s).fold(0.0f32, f32::max);
        let threshold = top * self.keep_ratio;

        // score desc, ties by original candidate order
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        scored
            .into_iter()
            .filter(|(_, s)| *s >= threshold)
            .take(self.max_hypotheses)
            .map(|(i, s)| Hypothesis {
                entity_id: candidates[i].0.clone(),
                label: candidates[i].1.clone(),
                score: s,
            })
            .collect()
    }
}

impl Default for Disambiguator {
    /// Three hypotheses, `keep_ratio = 0.7`.
    fn default() -> Self {
        Self::new(3)
    }
}

fn label_score(label: &str, ctx: &HashSet<String>) -> f32 {
    let toks = tokenize(label);
    let uniq: HashSet<&String> = toks.iter().collect();
    if uniq.is_empty() {
        return 0.0;
    }
    let hit = uniq.iter().filter(|t| ctx.contains(t.as_str())).count();
    hit as f32 / uniq.len() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_candidates_yields_empty() {
        let d = Disambiguator::new(3);
        assert!(d.resolve(&["apple".into()], "context", &[]).is_empty());
    }

    #[test]
    fn ambiguous_input_keeps_two_hypotheses_deterministically() {
        let d = Disambiguator::new(3);
        let cands = vec![
            ("Q312".to_string(), "Apple Inc".to_string()),
            ("Q89".to_string(), "apple fruit".to_string()),
            ("Q999".to_string(), "banana".to_string()),
        ];
        // Context supports both Apple readings fully; banana is unsupported.
        let ctx = "apple inc iphone macbook and apple fruit grows in orchards";
        let a = d.resolve(&["apple".into()], ctx, &cands);
        let b = d.resolve(&["apple".into()], ctx, &cands);
        assert_eq!(a.len(), 2, "ambiguous mention keeps both Apple readings");
        assert_eq!(a, b, "deterministic ordering");
        let ids: Vec<&str> = a.iter().map(|h| h.entity_id.as_str()).collect();
        // both score 1.0 -> tie broken by candidate order
        assert_eq!(ids, ["Q312", "Q89"]);
        assert!((a[0].score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn clear_winner_keeps_one() {
        let d = Disambiguator::new(3);
        let cands = vec![
            ("Q312".to_string(), "Apple Inc".to_string()),
            ("Q89".to_string(), "banana".to_string()),
        ];
        let ctx = "apple inc released a new iphone and macbook today";
        let out = d.resolve(&["apple".into()], ctx, &cands);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].entity_id, "Q312");
    }

    #[test]
    fn max_hypotheses_caps_kept_rivals() {
        let ctx = "apple apple apple";
        let cands = vec![
            ("a".to_string(), "apple".to_string()),
            ("b".to_string(), "apple".to_string()),
            ("c".to_string(), "apple".to_string()),
        ];
        let d = Disambiguator::new(2);
        assert_eq!(d.resolve(&[], ctx, &cands).len(), 2);
    }

    #[test]
    fn keep_ratio_is_configurable() {
        // "Apple Inc" -> 1/2 tokens in ctx; "banana" -> 1/1. top = 1.0.
        let cands = vec![
            ("inc".to_string(), "Apple Inc".to_string()),
            ("fruit".to_string(), "banana".to_string()),
        ];
        let ctx = "i ate a banana and read about apple";
        // default 0.7: 0.5 < 0.7 -> only banana survives
        assert_eq!(Disambiguator::new(3).resolve(&[], ctx, &cands).len(), 1);
        // keep_ratio 0.4: 0.5 >= 0.4 -> both survive
        assert_eq!(
            Disambiguator::with_keep_ratio(3, 0.4).resolve(&[], ctx, &cands).len(),
            2
        );
    }
}
