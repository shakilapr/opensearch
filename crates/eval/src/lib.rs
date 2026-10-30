//! Benchmarking: metrics, dataset loading, and the ablation runner (plan Stage 2).
//!
//! Gives hybrid retrieval a measuring stick — nDCG / MRR / recall over a committed query
//! set, with the resource budget recorded next to every number. Interfaces are frozen in
//! `docs/STAGE2_SPEC.md`.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize};

/// Errors from dataset loading and case validation.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    /// Filesystem / read failure.
    #[error("io error: {0}")]
    Io(String),
    /// Malformed JSON on a line.
    #[error("json error: {0}")]
    Json(String),
    /// A syntactically valid case that violates the schema.
    #[error("invalid case: {0}")]
    InvalidCase(String),
}

/// Query intent class. Mirrors the benchmark matrix in engine §29 / engine-arch §22.1.
///
/// Deserialization is tolerant: it accepts `short_factual`, `short-factual`,
/// `shortFactual` and `ShortFactual` alike (case- and separator-insensitive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryClass {
    ShortFactual,
    LongSemantic,
    AmbiguousEntity,
    Current,
    Papers,
    Docs,
    Github,
    Community,
    MultiHop,
    /// A conceptual query whose answer shares no content word with it — the oxytocin shape from
    /// PLAN-v2 §5. Kept distinct from `LongSemantic` because it is the class the semantic lanes
    /// exist for, and mixing it into a title-derived set hides whether they work.
    CrossDisciplinary,
}

impl QueryClass {
    fn parse(s: &str) -> Option<Self> {
        let norm: String = s
            .chars()
            .filter(|c| *c != '_' && *c != '-' && *c != ' ')
            .flat_map(|c| c.to_lowercase())
            .collect();
        Some(match norm.as_str() {
            "shortfactual" => Self::ShortFactual,
            "longsemantic" => Self::LongSemantic,
            "ambiguousentity" => Self::AmbiguousEntity,
            "current" => Self::Current,
            "papers" => Self::Papers,
            "docs" => Self::Docs,
            "github" => Self::Github,
            "community" => Self::Community,
            "crossdisciplinary" => Self::CrossDisciplinary,
            "multihop" => Self::MultiHop,
            _ => return None,
        })
    }
}

impl<'de> Deserialize<'de> for QueryClass {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(d)?;
        QueryClass::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown QueryClass: {s:?}")))
    }
}

/// One judged query with its relevance labels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryCase {
    pub id: String,
    pub query: String,
    pub class: QueryClass,
    /// Doc ids judged relevant (binary).
    #[serde(default)]
    pub relevant: Vec<String>,
    /// Optional graded labels 0..3, `(doc_id, gain)`.
    #[serde(default)]
    pub graded: Vec<(String, u8)>,
    /// Observed timestamp (unix seconds) used by [`split_by_time`].
    #[serde(default)]
    pub observed_at: Option<i64>,
}

/// Outcome of running one query through a retrieval configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunResult {
    pub case_id: String,
    /// Retrieved doc ids, best first.
    pub ranked: Vec<String>,
    pub latency_ms: u64,
}

/// Aggregate metrics across a set of joined results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub n: usize,
    pub ndcg_at_10: f64,
    pub mrr: f64,
    pub recall_at_10: f64,
    pub p50_ms: u64,
    pub p95_ms: u64,
}

/// Gain per doc id, resolving graded labels and deduping repeated ids (max gain wins).
fn gain_map(graded: &[(String, u8)]) -> HashMap<&str, u8> {
    let mut m: HashMap<&str, u8> = HashMap::with_capacity(graded.len());
    for (id, g) in graded {
        let e = m.entry(id.as_str()).or_insert(0);
        if *g > *e {
            *e = *g;
        }
    }
    m
}

/// Normalized discounted cumulative gain at `k`.
///
/// Convention (frozen; see `docs/STAGE2_SPEC.md` §eval):
/// * `gain(doc)` is the doc's graded label when present (`0..3`), used **linearly**; docs
///   absent from `graded` have gain `0`. `graded` is deduped by doc id (max wins).
/// * `DCG = Σ_{i=1..min(k,|ranked|)} gain(ranked[i]) / log2(i+1)`; a repeated doc id in
///   `ranked` contributes `0` after its first occurrence.
/// * `IDCG` = DCG of the ideal ordering of the **available graded docs** (their gains
///   sorted descending, capped at `k`).
/// * If `IDCG == 0`: `graded` empty (no relevance information) → `1.0` (vacuously
///   perfect); `graded` non-empty but with no positive gain → `0.0`.
/// * Otherwise `DCG / IDCG`.
///
/// Graded labels are the *only* relevance signal this function sees; the binary fallback
/// from [`QueryCase::relevant`] is applied by [`aggregate`], which synthesizes gain-1
/// labels when a case carries no `graded` list.
pub fn ndcg_at_k(ranked: &[String], graded: &[(String, u8)], k: usize) -> f64 {
    let gains = gain_map(graded);
    let has_judged = !gains.is_empty();

    // Ideal DCG: the best available ordering of the graded docs, capped at k.
    let mut ideal_gains: Vec<u8> = gains.values().copied().collect();
    ideal_gains.sort_unstable_by(|a, b| b.cmp(a));
    let idcg: f64 = ideal_gains
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, gain)| f64::from(*gain) / (i as f64 + 2.0).log2())
        .sum();

    if idcg == 0.0 {
        return if has_judged { 0.0 } else { 1.0 };
    }

    let mut seen: Vec<&str> = Vec::with_capacity(ranked.len().min(k));
    let mut dcg = 0.0f64;
    for (i, doc) in ranked.iter().take(k).enumerate() {
        if seen.contains(&doc.as_str()) {
            continue; // defensive: repeated ids gain nothing past the first hit
        }
        seen.push(doc.as_str());
        let gain = gains.get(doc.as_str()).copied().unwrap_or(0);
        dcg += f64::from(gain) / (i as f64 + 2.0).log2();
    }
    dcg / idcg
}

/// Reciprocal rank of the first ranked doc that is in `relevant` (0.0 if none).
pub fn mrr(ranked: &[String], relevant: &[String]) -> f64 {
    let rel: std::collections::HashSet<&str> = relevant.iter().map(String::as_str).collect();
    for (i, doc) in ranked.iter().enumerate() {
        if rel.contains(doc.as_str()) {
            return 1.0 / (i as f64 + 1.0);
        }
    }
    0.0
}

/// `|top-k ∩ relevant| / |relevant|`, counting unique ids; 0.0 when `relevant` is empty.
pub fn recall_at_k(ranked: &[String], relevant: &[String], k: usize) -> f64 {
    if relevant.is_empty() {
        return 0.0;
    }
    let rel: std::collections::HashSet<&str> = relevant.iter().map(String::as_str).collect();
    let mut hit: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for doc in ranked.iter().take(k) {
        if rel.contains(doc.as_str()) {
            hit.insert(doc.as_str());
        }
    }
    hit.len() as f64 / rel.len() as f64
}

/// Nearest-rank percentile over an ascending-sorted slice. Empty → 0.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len();
    let rank = (p / 100.0 * n as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(n - 1);
    sorted[idx]
}

/// Aggregate metrics over `results`, joined to `cases` by `case_id`.
///
/// Rule (documented + tested): `n` counts the [`RunResult`]s whose `case_id` matches a
/// case. Results referencing an unknown case are dropped; cases with no result are dropped
/// from every mean and from `n` — the metric is taken over what was actually run. Means use
/// `ndcg_at_k(.., 10)`, `mrr`, `recall_at_k(.., 10)`, with the binary fallback from
/// [`QueryCase::relevant`] when a case has no `graded` labels. Latencies are the `p50`/`p95`
/// nearest-rank percentiles of the joined results. `n == 0` yields all zeros.
pub fn aggregate(results: &[RunResult], cases: &[QueryCase]) -> Metrics {
    let by_id: HashMap<&str, &QueryCase> = cases.iter().map(|c| (c.id.as_str(), c)).collect();

    let mut ndcg_sum = 0.0;
    let mut mrr_sum = 0.0;
    let mut recall_sum = 0.0;
    let mut latencies: Vec<u64> = Vec::new();
    let mut n = 0usize;

    for r in results {
        let Some(case) = by_id.get(r.case_id.as_str()) else {
            continue;
        };
        n += 1;
        latencies.push(r.latency_ms);

        // Effective graded labels: real gradings when present, else binary gain 1 for
        // everything the case lists as relevant.
        let effective: Vec<(String, u8)> = if case.graded.is_empty() {
            case.relevant.iter().map(|d| (d.clone(), 1)).collect()
        } else {
            case.graded.clone()
        };

        ndcg_sum += ndcg_at_k(&r.ranked, &effective, 10);
        mrr_sum += mrr(&r.ranked, &case.relevant);
        recall_sum += recall_at_k(&r.ranked, &case.relevant, 10);
    }

    if n == 0 {
        return Metrics {
            n: 0,
            ndcg_at_10: 0.0,
            mrr: 0.0,
            recall_at_10: 0.0,
            p50_ms: 0,
            p95_ms: 0,
        };
    }

    latencies.sort_unstable();
    Metrics {
        n,
        ndcg_at_10: ndcg_sum / n as f64,
        mrr: mrr_sum / n as f64,
        recall_at_10: recall_sum / n as f64,
        p50_ms: percentile(&latencies, 50.0),
        p95_ms: percentile(&latencies, 95.0),
    }
}

/// Raw JSONL shape; tolerant of a missing `graded` / `observed_at`.
#[derive(Deserialize)]
struct RawCase {
    id: String,
    query: String,
    class: QueryClass,
    #[serde(default)]
    relevant: Vec<String>,
    #[serde(default)]
    graded: Vec<(String, u8)>,
    #[serde(default)]
    observed_at: Option<i64>,
}

/// Load JSONL query cases. Blank lines and `#` comment lines are skipped; a line whose
/// `query` is empty (or whitespace-only) is rejected with [`EvalError::InvalidCase`].
pub fn load_cases(path: &Path) -> Result<Vec<QueryCase>, EvalError> {
    let text = std::fs::read_to_string(path).map_err(|e| EvalError::Io(e.to_string()))?;
    let mut out = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let raw: RawCase = serde_json::from_str(trimmed)
            .map_err(|e| EvalError::Json(format!("line {}: {e}", lineno + 1)))?;
        if raw.query.trim().is_empty() {
            return Err(EvalError::InvalidCase(format!(
                "line {}: empty query (case id {:?})",
                lineno + 1,
                raw.id
            )));
        }
        out.push(QueryCase {
            id: raw.id,
            query: raw.query,
            class: raw.class,
            relevant: raw.relevant,
            graded: raw.graded,
            observed_at: raw.observed_at,
        });
    }
    Ok(out)
}

/// Time-split for leakage-free evaluation. `observed_at <= cutoff` (inclusive), and a
/// missing `observed_at`, go to the **static** set; everything strictly newer is
/// **dynamic**. Order is preserved within each half.
pub fn split_by_time(cases: &[QueryCase], cutoff_unix: i64) -> (Vec<QueryCase>, Vec<QueryCase>) {
    let mut statik = Vec::new();
    let mut dynamic = Vec::new();
    for c in cases {
        match c.observed_at {
            Some(t) if t > cutoff_unix => dynamic.push(c.clone()),
            _ => statik.push(c.clone()),
        }
    }
    (statik, dynamic)
}

impl fmt::Display for Metrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "n={} ndcg@10={:.4} mrr={:.4} recall@10={:.4} p50={}ms p95={}ms",
            self.n, self.ndcg_at_10, self.mrr, self.recall_at_10, self.p50_ms, self.p95_ms
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn v(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }
    fn g(pairs: &[(&str, u8)]) -> Vec<(String, u8)> {
        pairs.iter().map(|(s, x)| (s.to_string(), *x)).collect()
    }
    fn assert_close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-4, "expected {b}, got {a}");
    }

    // ---- nDCG ------------------------------------------------------------------------

    /// graded A=3, B=2, C=1; ranked [C, B, A]. Hand-computed:
    /// DCG  = 1/1 + 2/log2(3) + 3/log2(4) = 1 + 1.26186 + 1.5 = 3.7618595...
    /// IDCG = 3/1 + 2/log2(3) + 1/log2(4) = 3 + 1.26186 + 0.5 = 4.7618595...
    /// nDCG = 3.7618595 / 4.7618595 = 0.7899980...
    #[test]
    fn ndcg_graded_three_docs_hand_computed() {
        let ranked = v(&["C", "B", "A"]);
        let graded = g(&[("A", 3), ("B", 2), ("C", 1)]);
        assert_close(ndcg_at_k(&ranked, &graded, 10), 0.7899980);
    }

    #[test]
    fn ndcg_ideal_ordering_is_one() {
        let ranked = v(&["A", "B", "C"]);
        let graded = g(&[("A", 3), ("B", 2), ("C", 1)]);
        assert_close(ndcg_at_k(&ranked, &graded, 10), 1.0);
    }

    /// No graded labels at all → no relevance information → vacuously 1.0.
    #[test]
    fn ndcg_no_relevant_docs_is_one() {
        let ranked = v(&["A", "B"]);
        let empty: Vec<(String, u8)> = Vec::new();
        assert_close(ndcg_at_k(&ranked, &empty, 10), 1.0);
    }

    /// Judged docs exist but none carries positive gain → IDCG == 0 → 0.0.
    #[test]
    fn ndcg_judged_without_positive_gain_is_zero() {
        let graded = g(&[("A", 0), ("B", 0)]);
        assert_close(ndcg_at_k(&v(&["A", "B"]), &graded, 10), 0.0);
        assert_close(ndcg_at_k(&v(&["Z", "Y"]), &graded, 10), 0.0);
    }

    #[test]
    fn ndcg_respects_k() {
        // Ideal list [A(3),B(2),C(1)] but the ranking starts B,A,C.
        let ranked = v(&["B", "A", "C"]);
        let graded = g(&[("A", 3), ("B", 2), ("C", 1)]);
        // DCG@1 = 2/1 = 2 ; IDCG@1 = 3/1 = 3 → 2/3.
        assert_close(ndcg_at_k(&ranked, &graded, 1), 2.0 / 3.0);
        // k=2 : DCG = 2 + 3/log2(3) ; IDCG = 3 + 2/log2(3).
        let dcg = 2.0 + 3.0 / 3f64.log2();
        let idcg = 3.0 + 2.0 / 3f64.log2();
        assert_close(ndcg_at_k(&ranked, &graded, 2), dcg / idcg);
    }

    /// A repeated doc id in `ranked` gains nothing after its first occurrence.
    #[test]
    fn ndcg_dedupes_repeated_ranked_ids() {
        let graded = g(&[("A", 1)]);
        assert_close(ndcg_at_k(&v(&["A", "B"]), &graded, 10), 1.0);
        assert_close(ndcg_at_k(&v(&["A", "A", "B"]), &graded, 10), 1.0);
        // Duplicate in `graded` also dedupes (max gain wins).
        assert_close(ndcg_at_k(&v(&["A"]), &g(&[("A", 1), ("A", 3)]), 10), 1.0);
    }

    // ---- MRR -------------------------------------------------------------------------

    #[test]
    fn mrr_first_hit() {
        assert_close(mrr(&v(&["A", "B"]), &v(&["A"])), 1.0);
    }

    #[test]
    fn mrr_second_hit() {
        assert_close(mrr(&v(&["X", "A", "B"]), &v(&["A"])), 0.5);
    }

    #[test]
    fn mrr_none() {
        assert_close(mrr(&v(&["X", "Y"]), &v(&["A"])), 0.0);
    }

    // ---- recall ----------------------------------------------------------------------

    #[test]
    fn recall_fewer_than_k_results() {
        // Only 2 results exist; 2 of 3 relevant found within k=10.
        assert_close(recall_at_k(&v(&["A", "B"]), &v(&["A", "B", "C"]), 10), 2.0 / 3.0);
    }

    #[test]
    fn recall_caps_at_k() {
        // Relevant C sits at position 3; k=2 misses it.
        assert_close(recall_at_k(&v(&["A", "B", "C"]), &v(&["A", "C"]), 2), 0.5);
        assert_close(recall_at_k(&v(&["A", "B", "C"]), &v(&["A", "C"]), 10), 1.0);
    }

    #[test]
    fn recall_empty_relevant_is_zero() {
        assert_close(recall_at_k(&v(&["A"]), &v(&[]), 10), 0.0);
    }

    // ---- aggregate -------------------------------------------------------------------

    #[test]
    fn aggregate_means_and_percentiles() {
        let cases = vec![
            case("q1", &["A"], &[]),
            case("q2", &["A"], &[]),
            case("q3", &["A"], &[]),
        ];
        // q1: A at rank 1 → ndcg 1, mrr 1, recall 1. q2: miss → all 0. q3: A at rank 3.
        let results = vec![
            run("q1", &["A", "B"], 10),
            run("q2", &["X", "Y"], 20),
            run("q3", &["B", "Y", "A"], 30),
        ];
        let m = aggregate(&results, &cases);
        assert_eq!(m.n, 3);
        // ndcg q3: DCG = 1/log2(4) = 0.5 ; IDCG = 1/log2(2) = 1 → 0.5.
        assert_close(m.ndcg_at_10, (1.0 + 0.0 + 0.5) / 3.0);
        assert_close(m.mrr, (1.0 + 0.0 + 1.0 / 3.0) / 3.0);
        assert_close(m.recall_at_10, (1.0 + 0.0 + 1.0) / 3.0);
        // latencies [10,20,30]: nearest-rank p50 = ceil(0.5*3)=2 → idx1 = 20;
        // p95 = ceil(0.95*3)=3 → idx2 = 30.
        assert_eq!(m.p50_ms, 20);
        assert_eq!(m.p95_ms, 30);
    }

    #[test]
    fn aggregate_larger_percentile_vector() {
        let cases: Vec<QueryCase> = (0..10)
            .map(|i| case(&format!("q{i}"), &["A"], &[]))
            .collect();
        let results: Vec<RunResult> = (0..10)
            .map(|i| run(&format!("q{i}"), &["A"], ((i + 1) * 10) as u64))
            .collect();
        let m = aggregate(&results, &cases);
        assert_eq!(m.n, 10);
        assert_eq!(m.p50_ms, 50); // ceil(5.0)=5 → idx4
        assert_eq!(m.p95_ms, 100); // ceil(9.5)=10 → idx9
    }

    /// `n` counts only results that join to a case; unjoined results and result-less cases
    /// are dropped from both the means and `n`.
    #[test]
    fn aggregate_counts_only_joined_results() {
        let cases = vec![case("q1", &["A"], &[]), case("q2", &["A"], &[])];
        let results = vec![
            run("q1", &["A"], 5),    // joins
            run("ghost", &["A"], 9), // no case → dropped
        ];
        let m = aggregate(&results, &cases);
        assert_eq!(m.n, 1);
        assert_close(m.ndcg_at_10, 1.0);
        assert_close(m.mrr, 1.0);
        assert_close(m.recall_at_10, 1.0);
        assert_eq!(m.p50_ms, 5);
        assert_eq!(m.p95_ms, 5);
    }

    #[test]
    fn aggregate_empty_is_all_zero() {
        let m = aggregate(&[], &[]);
        assert_eq!(m.n, 0);
        assert_eq!(m.p50_ms, 0);
        assert_eq!(m.p95_ms, 0);
        assert_close(m.ndcg_at_10, 0.0);
    }

    /// A case with no `graded` list falls back to binary gain 1 from `relevant`; a case
    /// with no judgments scores a perfect nDCG.
    #[test]
    fn aggregate_binary_fallback_and_no_judgement() {
        let cases = vec![case("q1", &["A", "B"], &[]), case("q2", &[], &[])];
        let results = vec![run("q1", &["A", "Z"], 1), run("q2", &["X"], 1)];
        let m = aggregate(&results, &cases);
        // q1: DCG = 1/1 (A) = 1 ; IDCG = 1/1 + 1/log2(3) → ndcg = 1/idcg.
        let idcg = 1.0 + 1.0 / 3f64.log2();
        assert_close(m.ndcg_at_10, (1.0 / idcg + 1.0) / 2.0);
    }

    // ---- load_cases ------------------------------------------------------------------

    #[test]
    fn load_cases_parses_skips_blank_and_comment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cases.jsonl");
        let body = "\
# a comment line\n\
\n\
{\"id\":\"q1\",\"query\":\"rust lifetimes\",\"class\":\"short_factual\",\"relevant\":[\"d1\",\"d2\"],\"graded\":[[\"d1\",3],[\"d2\",1]],\"observed_at\":100}\n\
{\"id\":\"q2\",\"query\":\"how does rrf work\",\"class\":\"LongSemantic\"}\n\
   \n\
{\"id\":\"q3\",\"query\":\"apple\",\"class\":\"ambiguous-entity\",\"relevant\":[\"d9\"]}\n";
        std::fs::write(&path, body).unwrap();
        let cases = load_cases(&path).unwrap();
        assert_eq!(cases.len(), 3);
        assert_eq!(cases[0].id, "q1");
        assert_eq!(cases[0].class, QueryClass::ShortFactual);
        assert_eq!(
            cases[0].graded,
            vec![("d1".to_string(), 3), ("d2".to_string(), 1)]
        );
        assert_eq!(cases[0].observed_at, Some(100));
        // Missing graded / observed_at tolerated.
        assert_eq!(cases[1].class, QueryClass::LongSemantic);
        assert!(cases[1].graded.is_empty());
        assert_eq!(cases[1].observed_at, None);
        // kebab / mixed-case class names accepted.
        assert_eq!(cases[2].class, QueryClass::AmbiguousEntity);
    }

    #[test]
    fn load_cases_rejects_empty_query() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cases.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{{\"id\":\"bad\",\"query\":\"   \",\"class\":\"docs\"}}").unwrap();
        match load_cases(&path) {
            Err(EvalError::InvalidCase(msg)) => assert!(msg.contains("empty query"), "{msg}"),
            other => panic!("expected InvalidCase, got {other:?}"),
        }
    }

    #[test]
    fn load_cases_bad_json_line_is_json_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cases.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{{\"id\":\"q1\",\"query\":\"ok\",\"class\":\"docs\"}}").unwrap();
        writeln!(f, "{{not valid json").unwrap();
        match load_cases(&path) {
            Err(EvalError::Json(msg)) => assert!(msg.contains("line 2"), "{msg}"),
            other => panic!("expected Json error, got {other:?}"),
        }
    }

    #[test]
    fn load_cases_missing_file_is_io_error() {
        let dir = tempfile::tempdir().unwrap();
        match load_cases(&dir.path().join("nope.jsonl")) {
            Err(EvalError::Io(_)) => {}
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    // ---- split_by_time ---------------------------------------------------------------

    #[test]
    fn split_by_time_boundaries() {
        let cases = vec![
            timed("old", Some(50)),
            timed("cut", Some(100)), // == cutoff → static (inclusive)
            timed("new", Some(101)), // > cutoff → dynamic
            timed("none", None),     // missing → static
        ];
        let (statik, dynamic) = split_by_time(&cases, 100);
        let sids: Vec<&str> = statik.iter().map(|c| c.id.as_str()).collect();
        let dids: Vec<&str> = dynamic.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(sids, vec!["old", "cut", "none"]);
        assert_eq!(dids, vec!["new"]);
    }

    // ---- helpers ---------------------------------------------------------------------

    fn case(id: &str, relevant: &[&str], graded: &[(&str, u8)]) -> QueryCase {
        QueryCase {
            id: id.to_string(),
            query: format!("query {id}"),
            class: QueryClass::Docs,
            relevant: relevant.iter().map(|s| s.to_string()).collect(),
            graded: graded.iter().map(|(s, x)| (s.to_string(), *x)).collect(),
            observed_at: None,
        }
    }

    fn run(case_id: &str, ranked: &[&str], latency_ms: u64) -> RunResult {
        RunResult {
            case_id: case_id.to_string(),
            ranked: ranked.iter().map(|s| s.to_string()).collect(),
            latency_ms,
        }
    }

    fn timed(id: &str, observed_at: Option<i64>) -> QueryCase {
        QueryCase {
            id: id.to_string(),
            query: format!("query {id}"),
            class: QueryClass::Current,
            relevant: Vec::new(),
            graded: Vec::new(),
            observed_at,
        }
    }
}
