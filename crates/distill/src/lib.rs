//! Policy distillation (plan Stage 9): a deterministic linear policy trained from traces.
//! Interfaces frozen in `docs/STAGE9_SPEC.md`.
//!
//! Honest limits (mandatory, see the Stage-9 spec): real SPLADE/BGE weights and a trained
//! cross-encoder are **not** available in this environment. The only thing trained here is a
//! tiny multinomial **linear** classifier over hand-designed features, fit from a committed
//! trace fixture. Any artifact-backed path returns [`DistillError::Unavailable`] instead of
//! inventing numbers. Measured accuracy is returned verbatim, even when it is below 1.0.
//!
//! Trace dependency: `distill_from_trace` reads the trace with the Stage-8
//! `search_trace::read_trace` reader (the frozen `TraceEvent` wire form), not a local parser. The
//! committed Stage-9 fixture predates the frozen snake_case `phase` tag (it writes `"Route"`), so
//! a tiny adapter normalises the phase casing and re-reads through `read_trace`; the trace schema
//! itself is never weakened.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

/// Number of features produced by [`features_from_state`]. Fixed so the policy's weight vector
/// has a stable shape.
pub const FEATURE_DIM: usize = 6;

/// Errors surfaced by distillation.
#[derive(Debug, thiserror::Error)]
pub enum DistillError {
    /// The trace file could not be read.
    #[error("failed to read trace: {0}")]
    Io(#[from] std::io::Error),
    /// A trace line was not valid JSON / not an object.
    #[error("failed to parse trace: {0}")]
    Parse(String),
    /// The trace contained no usable labelled samples.
    #[error("trace contains no labelled samples")]
    Empty,
    /// An artifact-backed (learned-weights) path was requested but no such artifact exists in
    /// this environment. This is deliberate and honest — never replaced by a fake number.
    #[error("unavailable: {0}")]
    Unavailable(String),
}

/// Hand-designed feature vector extracted from a query's decision state.
///
/// Six features, always in this order:
/// `1.` query token count (log1p), `2.` query char length (log1p), `3.` question flag,
/// `4.` graph hops (as f32), `5.` candidate count (log1p), `6.` ambiguity in `[0, 1]`.
/// Deterministic and input-length independent, so a linear policy has a stable shape.
pub fn features_from_state(query: &str, hops: u8, candidates: usize, ambiguity: f32) -> Vec<f32> {
    let tokens = query.split_whitespace().count();
    let chars = query.chars().count();
    let question = if query.trim_end().ends_with('?') { 1.0 } else { 0.0 };
    let ambiguity = if ambiguity.is_finite() {
        ambiguity.clamp(0.0, 1.0)
    } else {
        0.0
    };
    vec![
        (tokens as f32 + 1.0).ln(),
        (chars as f32 + 1.0).ln(),
        question,
        hops as f32,
        (candidates as f32 + 1.0).ln(),
        ambiguity,
    ]
}

/// A multinomial linear classifier over a fixed feature dimension.
///
/// `weights` is class-major flattened: class `c`'s weight vector occupies
/// `weights[c * dim .. (c + 1) * dim]`. `bias` is a single shared intercept (added to every
/// class), which this crate's softmax training leaves at its initial value because a common
/// offset cancels in the softmax — it is kept to honour the frozen struct shape.
#[derive(Debug, Clone, PartialEq)]
pub struct LinearPolicy {
    pub weights: Vec<f32>,
    pub bias: f32,
    /// Feature dimension.
    pub dim: usize,
    /// Class labels in index order (matches the `weights` blocks).
    pub classes: Vec<String>,
}

impl LinearPolicy {
    /// Train with deterministic full-batch gradient descent (softmax cross-entropy).
    ///
    /// Same `(samples, classes, epochs, lr)` always yields the same weights: zero initialisation,
    /// fixed sample order, no shuffling, no RNG. Samples whose feature length differs from the
    /// first sample's are skipped.
    pub fn train(
        samples: &[(Vec<f32>, String)],
        classes: &[String],
        epochs: usize,
        lr: f32,
    ) -> Self {
        let dim = samples.first().map(|(x, _)| x.len()).unwrap_or(0);
        let nclasses = classes.len();
        let mut weights = vec![0.0f32; nclasses * dim];
        let mut bias = 0.0f32;
        if dim == 0 || nclasses == 0 {
            return LinearPolicy {
                weights,
                bias,
                dim,
                classes: classes.to_vec(),
            };
        }
        let index: std::collections::BTreeMap<&str, usize> =
            classes.iter().enumerate().map(|(i, c)| (c.as_str(), i)).collect();

        for _ in 0..epochs {
            for (x, label) in samples {
                if x.len() != dim {
                    continue;
                }
                let y = match index.get(label.as_str()) {
                    Some(&i) => i,
                    None => continue, // unknown label: ignored
                };
                let logits = Self::self_logits(&weights, dim, x, bias);
                let probs = softmax(&logits);
                for (c, p) in probs.iter().enumerate() {
                    let g = p - if c == y { 1.0 } else { 0.0 };
                    let base = c * dim;
                    for d in 0..dim {
                        weights[base + d] -= lr * g * x[d];
                    }
                    // Common bias: sum of (p - y) over classes is 0, so it stays put.
                    bias -= lr * g;
                }
            }
        }

        LinearPolicy {
            weights,
            bias,
            dim,
            classes: classes.to_vec(),
        }
    }

    fn self_logits(weights: &[f32], dim: usize, x: &[f32], bias: f32) -> Vec<f32> {
        let nclasses = weights.len() / dim;
        (0..nclasses)
            .map(|c| {
                let base = c * dim;
                let mut s = bias;
                for d in 0..dim {
                    s += weights[base + d] * x[d];
                }
                s
            })
            .collect()
    }

    /// Predict the class for `x`. Ties break to the lowest class index (deterministic).
    pub fn predict(&self, x: &[f32]) -> String {
        if self.classes.is_empty() {
            return String::new();
        }
        let dim = self.dim.max(1);
        let logits = Self::self_logits(&self.weights, dim, x, self.bias);
        let mut best = 0usize;
        for (c, s) in logits.iter().enumerate() {
            if *s > logits[best] {
                best = c;
            }
        }
        self.classes[best].clone()
    }
}

fn softmax(logits: &[f32]) -> Vec<f32> {
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = logits.iter().map(|s| (s - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    if sum == 0.0 {
        vec![1.0 / logits.len().max(1) as f32; logits.len()]
    } else {
        exps.into_iter().map(|e| e / sum).collect()
    }
}

/// Training hyper-parameters for the trace distillation fixture (fixed → deterministic).
const DISTILL_EPOCHS: usize = 400;
const DISTILL_LR: f32 = 0.5;

/// Distill a tier policy from a committed trace fixture and return the **measured** accuracy on
/// that fixture (correct predictions / samples), in `[0, 1]`.
///
/// Reads a JSONL trace; each line's `policy_tier` is the label and `detail.{query,hops,
/// candidates,ambiguity}` supplies the feature inputs. Trains a [`LinearPolicy`] over the label
/// set (sorted, so class order is deterministic) and scores it on the same fixture. Returns
/// [`DistillError::Empty`] when no labelled samples are present.
pub fn distill_from_trace(path: &Path) -> Result<f32, DistillError> {
    let events = read_trace_compat(path)?;
    let mut samples: Vec<(Vec<f32>, String)> = Vec::new();
    for ev in &events {
        let label = match &ev.policy_tier {
            Some(l) if !l.is_empty() => l.clone(),
            _ => continue,
        };
        let x = features_from_state(
            ev.detail.get("query").map(String::as_str).unwrap_or(""),
            detail_u8(ev, "hops"),
            detail_usize(ev, "candidates"),
            detail_f32(ev, "ambiguity"),
        );
        samples.push((x, label));
    }
    if samples.is_empty() {
        return Err(DistillError::Empty);
    }
    let classes: Vec<String> = samples
        .iter()
        .map(|(_, l)| l.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let policy = LinearPolicy::train(&samples, &classes, DISTILL_EPOCHS, DISTILL_LR);
    let correct = samples
        .iter()
        .filter(|(x, y)| &policy.predict(x) == y)
        .count();
    Ok(correct as f32 / samples.len() as f32)
}

fn detail_f32(ev: &search_trace::TraceEvent, key: &str) -> f32 {
    ev.detail
        .get(key)
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(0.0)
}

fn detail_u8(ev: &search_trace::TraceEvent, key: &str) -> u8 {
    ev.detail
        .get(key)
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(0)
}

fn detail_usize(ev: &search_trace::TraceEvent, key: &str) -> usize {
    ev.detail
        .get(key)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0)
}

/// Read a trace through the frozen [`search_trace::read_trace`].
///
/// The committed Stage-9 fixture predates the frozen wire form: it tags `phase`
/// as PascalCase (`"Route"`) whereas `search_trace::Phase` deserialises the
/// snake_case form (`"route"`). Rather than duplicate a parser or weaken the
/// schema, this adapter normalises only the `phase` casing and hands the
/// canonical JSONL back to `read_trace`. A trace that is canonical to begin with
/// is read directly.
fn read_trace_compat(path: &Path) -> Result<Vec<search_trace::TraceEvent>, DistillError> {
    match search_trace::read_trace(path) {
        Ok(events) => Ok(events),
        Err(first_err) => {
            let text = std::fs::read_to_string(path)?;
            let mut normalized = String::with_capacity(text.len());
            let mut changed = false;
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match normalize_phase_case(trimmed) {
                    Some(fixed) => {
                        changed = true;
                        normalized.push_str(&fixed);
                    }
                    None => normalized.push_str(trimmed),
                }
                normalized.push('\n');
            }
            if !changed {
                // Not a phase-casing difference: the file is genuinely malformed
                // for the frozen schema. Surface the original read error.
                return Err(DistillError::Io(first_err));
            }
            let tmp = std::env::temp_dir().join(format!(
                "search_distill_trace_{}_{}.jsonl",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::write(&tmp, normalized)?;
            let result = search_trace::read_trace(&tmp);
            let _ = std::fs::remove_file(&tmp);
            Ok(result?)
        }
    }
}

/// Rewrite a trace line's `phase` tag to lowercase when it is PascalCase.
/// Returns `None` when the line needs no change (or is not a JSON object).
fn normalize_phase_case(line: &str) -> Option<String> {
    let mut v: Value = serde_json::from_str(line).ok()?;
    let phase = v.get("phase")?.as_str()?.to_string();
    let lowered = phase.to_ascii_lowercase();
    if lowered == phase {
        return None;
    }
    *v.get_mut("phase")? = Value::String(lowered);
    serde_json::to_string(&v).ok()
}

/// Artifact-backed distillation is **unavailable** in this environment.
///
/// Real SPLADE/BGE sparse weights and a trained cross-encoder are not shipped with this
/// repository (see the Stage-2 bench and `docs/STAGE9_SPEC.md`). This function always returns
/// [`DistillError::Unavailable`] — it never fabricates a model or an accuracy number.
pub fn distill_from_artifact(path: &Path) -> Result<LinearPolicy, DistillError> {
    Err(DistillError::Unavailable(format!(
        "no learned distillation artifact at {}: real SPLADE/BGE weights and a trained \
         cross-encoder are not available in this environment (honest-limits rule)",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(x: &[f32], label: &str) -> (Vec<f32>, String) {
        (x.to_vec(), label.to_string())
    }

    #[test]
    fn features_are_fixed_dim_and_deterministic() {
        let a = features_from_state("why is the sky blue?", 2, 40, 0.8);
        let b = features_from_state("why is the sky blue?", 2, 40, 0.8);
        assert_eq!(a, b);
        assert_eq!(a.len(), FEATURE_DIM);
        assert!(a.iter().all(|x| x.is_finite()));
        // Question flag fires.
        assert_eq!(a[2], 1.0);
        // Ambiguity is clamped.
        let c = features_from_state("x", 0, 0, 5.0);
        assert_eq!(c[5], 1.0);
    }

    #[test]
    fn linear_policy_trains_deterministically() {
        let classes = vec!["cheap".to_string(), "deep".to_string()];
        let samples = vec![
            sample(&[0.0, 0.0], "cheap"),
            sample(&[1.0, 0.0], "deep"),
            sample(&[0.1, 0.1], "cheap"),
            sample(&[1.1, 0.9], "deep"),
        ];
        let p1 = LinearPolicy::train(&samples, &classes, 200, 0.5);
        let p2 = LinearPolicy::train(&samples, &classes, 200, 0.5);
        assert_eq!(p1.weights, p2.weights, "same input must give same weights");
        assert_eq!(p1.bias, p2.bias);
        // And it actually separates the two clusters.
        assert_eq!(p1.predict(&[0.0, 0.0]), "cheap");
        assert_eq!(p1.predict(&[1.0, 1.0]), "deep");
    }

    #[test]
    fn artifact_path_is_honestly_unavailable() {
        let err = distill_from_artifact(Path::new("/nonexistent/model.safetensors")).unwrap_err();
        assert!(matches!(err, DistillError::Unavailable(_)));
        let msg = err.to_string();
        assert!(msg.contains("unavailable"));
    }

    #[test]
    fn empty_trace_is_an_error() {
        let dir = std::env::temp_dir().join("search_distill_empty_test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("empty.jsonl");
        std::fs::write(&p, "\n\n").unwrap();
        assert!(matches!(distill_from_trace(&p), Err(DistillError::Empty)));
    }

    #[test]
    fn canonical_trace_parses_through_read_trace() {
        // A trace in the frozen STAGE8 wire form (snake_case phase) is read by
        // `search_trace::read_trace` with no adapter, and distils.
        use std::collections::BTreeMap;
        let dir = std::env::temp_dir().join("search_distill_canonical_test");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("canonical.jsonl");
        let mut lines = String::new();
        for (tier, ambiguity) in [("cheap", 0.0f32), ("deep", 1.0f32)] {
            for i in 0..4 {
                let detail = BTreeMap::from([
                    ("query".to_string(), "alpha beta".to_string()),
                    ("hops".to_string(), "0".to_string()),
                    ("candidates".to_string(), "1".to_string()),
                    ("ambiguity".to_string(), ambiguity.to_string()),
                ]);
                let ev = search_trace::TraceEvent {
                    ts_ms: 0,
                    query_id: format!("q-{tier}-{i}"),
                    phase: search_trace::Phase::Route,
                    action: "choose_tier".to_string(),
                    detail,
                    policy_tier: Some(tier.to_string()),
                    cost_ms: 0,
                };
                lines.push_str(&serde_json::to_string(&ev).unwrap());
                lines.push('\n');
            }
        }
        std::fs::write(&p, lines).unwrap();
        let acc = distill_from_trace(&p).unwrap();
        assert!(acc > 0.5, "canonical trace distils, got {acc}");
    }
}
