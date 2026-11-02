//! Ablation harness (plan Stage 2 exit gate).
//!
//! Builds a temp Tantivy index from the committed fixture corpus (`benchmarks/corpus`), runs
//! the query set (`benchmarks/queries.example.jsonl`) through every required lane combination,
//! and reports nDCG@10 / MRR / recall@10 plus a latency + candidate budget. It never invents a
//! number: any lane that cannot be honestly populated in this environment is reported as
//! UNAVAILABLE in the output (see [`Report::notes`]).
//!
//! This is the library half of the crate. The `bench` binary is a thin CLI over [`run`]; the
//! tests drive the very same function, so what ships is what is measured.
//!
//! The dense lane's embedder is **selectable** via [`Config::embedder`] / `--embedder hash|bge`:
//! the deterministic [`HashEmbedder`] (the default — a *lexical* hashed bag-of-words), or the real
//! BGE-small-en-v1.5 model behind `feature = "bge"` (a *semantic* model, 384-d). Every lane label
//! and honesty note names the embedder actually in use, so the report can never claim a semantic
//! lane it did not run. `--dim` sets the dimension of the hash embedder only; the reported
//! `budget.embed_dim` is always the *actual* dimension of the selected embedder.
//!
//! Ablations (see `docs/specs/STAGE2_SPEC.md`):
//! * `bm25`                          — global lexical BM25 over the Tantivy index.
//! * `bm25+dense`                    — RRF(bm25, selected-embedder top-k over title+body).
//! * `bm25+dense+sparse`             — RRF(bm25, dense, source-routed BM25 lane).
//! * `source-routing`                — source-routed BM25 only (no global lane).
//! * `source-routing+global`         — RRF(source-routed, global BM25).
//! * `bm25+ce-on`                    — bm25 candidates reranked by the lexical cross-encoder.
//! * `bm25+dense+sparse+ce-on`       — full hybrid reranked by the lexical cross-encoder.
//!
//! The `ce-off` counterpart of each `ce-on` row is its parent hybrid row (cross-encoder on/off).

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use search_embed::{top_k, Embedder, Embedding, HashEmbedder};
use search_eval::{aggregate, load_cases, Metrics, QueryCase, RunResult};
use search_indexer::{IndexDoc, Indexer, IndexerConfig};
use search_parser::normalize;
use search_rerank::{CrossEncoder, LexicalCrossEncoder, Rrf};
use search_retrieval::{merge_dedup, LaneSearcher, MemoryDoc, MemoryLaneSearcher};

/// Errors surfaced by the harness.
#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error("io error: {0}")]
    Io(String),
    #[error("index error: {0}")]
    Index(String),
    #[error("eval error: {0}")]
    Eval(String),
    #[error("json error: {0}")]
    Json(String),
    #[error("embedder error: {0}")]
    Embed(String),
}

impl From<std::io::Error> for BenchError {
    fn from(e: std::io::Error) -> Self {
        BenchError::Io(e.to_string())
    }
}
impl From<serde_json::Error> for BenchError {
    fn from(e: serde_json::Error) -> Self {
        BenchError::Json(e.to_string())
    }
}
impl From<search_eval::EvalError> for BenchError {
    fn from(e: search_eval::EvalError) -> Self {
        BenchError::Eval(e.to_string())
    }
}
impl From<search_core::AppError> for BenchError {
    fn from(e: search_core::AppError) -> Self {
        BenchError::Index(e.to_string())
    }
}

/// One parsed fixture document.
#[derive(Debug, Clone)]
pub struct CorpusDoc {
    /// Stable id derived from the corpus-relative path, e.g. `arxiv/attention`.
    pub id: String,
    /// First path component under `corpus/`, e.g. `arxiv`.
    pub source: String,
    pub title: String,
    pub body: String,
    pub url: String,
}

/// Which dense embedder the `bm25+dense*` lanes run with.
///
/// [`HashEmbedder`] is the deterministic default: a *lexical* hashed bag-of-words, **not** a
/// semantic model. [`EmbedderKind::Bge`] selects the real BGE-small-en-v1.5 model, which is only
/// compiled in under `feature = "bge"`; in a default build it fails with an actionable rebuild
/// message rather than silently falling back to hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedderKind {
    /// Deterministic hashed bag-of-words (lexical). The default.
    Hash,
    /// Real BGE-small-en-v1.5 (semantic, 384-d); requires `feature = "bge"`.
    Bge,
}

impl EmbedderKind {
    /// Parse a `--embedder` value. Case-insensitive.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "hash" => Ok(EmbedderKind::Hash),
            "bge" => Ok(EmbedderKind::Bge),
            other => Err(format!(
                "unknown embedder `{other}` (expected `hash` or `bge`)"
            )),
        }
    }

    /// Canonical CLI spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            EmbedderKind::Hash => "hash",
            EmbedderKind::Bge => "bge",
        }
    }

    /// Short name used in lane labels (e.g. `RRF(bm25, <short> top-k ...)`).
    pub fn short_name(self) -> &'static str {
        match self {
            EmbedderKind::Hash => "HashEmbedder",
            EmbedderKind::Bge => "BGE-small-en-v1.5",
        }
    }

    /// Full display name recorded in the report (`budget.embedder`), stating exactly what the
    /// dense lane is — and, for hash, that it is *not* a semantic model.
    pub fn display_name(self) -> &'static str {
        match self {
            EmbedderKind::Hash => {
                "HashEmbedder (deterministic hashed bag-of-words; lexical, NOT a semantic model)"
            }
            EmbedderKind::Bge => "BGE-small-en-v1.5 (384-d, CLS-pooled, L2-normalized; semantic)",
        }
    }
}

impl Default for EmbedderKind {
    fn default() -> Self {
        EmbedderKind::Hash
    }
}

impl std::str::FromStr for EmbedderKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        EmbedderKind::parse(s)
    }
}

/// Build the lane's embedder from the config. This is the single place that maps a
/// [`EmbedderKind`] to a concrete [`Embedder`].
///
/// * [`EmbedderKind::Hash`] → [`HashEmbedder::new(cfg.dim)`].
/// * [`EmbedderKind::Bge`] with `feature = "bge"` → the real BGE embedder; construction can fail
///   (e.g. missing weights), and that error is propagated, never swallowed.
/// * [`EmbedderKind::Bge`] without `feature = "bge"` → an actionable error telling the caller how
///   to rebuild; the harness never silently falls back to hash.
pub fn build_embedder(cfg: &Config) -> Result<Box<dyn Embedder>, BenchError> {
    match cfg.embedder {
        EmbedderKind::Hash => Ok(Box::new(HashEmbedder::new(cfg.dim))),
        #[cfg(feature = "bge")]
        EmbedderKind::Bge => {
            let e = search_embed::bge::BgeEmbedder::new()
                .map_err(|e| BenchError::Embed(format!("could not build the bge embedder: {e}")))?;
            Ok(Box::new(e))
        }
        #[cfg(not(feature = "bge"))]
        EmbedderKind::Bge => Err(BenchError::Embed(
            "the bge embedder requires rebuilding: \
             cargo run -p search-bench --release --features bge -- --embedder bge"
                .into(),
        )),
    }
}

/// Harness configuration. Paths default to the committed `benchmarks/` tree.
#[derive(Debug, Clone)]
pub struct Config {
    pub corpus_dir: PathBuf,
    pub queries_path: PathBuf,
    pub out_path: PathBuf,
    /// Ranked-list depth kept per query.
    pub k: usize,
    /// Dimension for the hash embedder. Ignored by the BGE embedder (always 384).
    pub dim: usize,
    /// Which dense embedder the `bm25+dense*` lanes use.
    pub embedder: EmbedderKind,
    /// How many top candidates the cross-encoder reranks.
    pub ce_depth: usize,
}

/// Repository-relative default paths, resolved at compile time from the crate location.
pub const DEFAULT_CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../benchmarks/corpus");
pub const DEFAULT_QUERIES: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../benchmarks/queries.example.jsonl");
pub const DEFAULT_OUT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../benchmarks/results.json");

impl Default for Config {
    fn default() -> Self {
        Config {
            corpus_dir: PathBuf::from(DEFAULT_CORPUS_DIR),
            queries_path: PathBuf::from(DEFAULT_QUERIES),
            out_path: PathBuf::from(DEFAULT_OUT),
            k: 20,
            dim: 1024,
            embedder: EmbedderKind::Hash,
            ce_depth: 50,
        }
    }
}

/// Recorded resource budget for a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Budget {
    pub docs: usize,
    pub queries: usize,
    pub k: usize,
    pub ce_depth: usize,
    pub embed_dim: usize,
    /// Display name of the dense embedder actually used for this run (see [`EmbedderKind::display_name`]).
    pub embedder: String,
    /// Resident set size sampled from `/proc/self/statm` at report time (Linux only).
    pub peak_rss_kb: u64,
    /// Mean number of candidates each ablation scored per query, and the max.
    pub mean_candidates: f64,
    pub max_candidates: usize,
}

/// One ablation row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AblationRow {
    pub name: String,
    /// Human note: how the lane was populated, or why it could not be.
    pub note: String,
    pub metrics: Metrics,
    pub mean_candidates: f64,
    pub max_candidates: usize,
}

/// The machine-readable report written to `benchmarks/results.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub version: String,
    pub generated_unix: i64,
    pub corpus_dir: String,
    pub queries_path: String,
    pub budget: Budget,
    /// Honesty notes: unavailable lanes, smoke-set caveats, routing policy.
    pub notes: Vec<String>,
    pub ablations: Vec<AblationRow>,
    /// Per-case bm25 vs hybrid rank of the first relevant doc (diagnostic).
    pub per_case: Vec<CaseRank>,
}

// ---------------------------------------------------------------------------------------------
// Corpus loading
// ---------------------------------------------------------------------------------------------

fn collect_html(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) -> Result<(), BenchError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            collect_html(&path, &child, out)?;
        } else if name.ends_with(".html") {
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            out.push((child, path));
        }
    }
    Ok(())
}

/// Load every `*.html` under `dir`, normalizing with [`search_parser::normalize`].
/// Doc id is the corpus-relative path without extension; source is its first component.
pub fn load_corpus(dir: &Path) -> Result<Vec<CorpusDoc>, BenchError> {
    let mut files = Vec::new();
    collect_html(dir, "", &mut files)?;
    files.sort(); // deterministic order

    let mut docs = Vec::with_capacity(files.len());
    for (rel, path) in files {
        let bytes = fs::read(&path)?;
        let url = format!("https://corpus.example/{rel}");
        let nd = normalize(&url, &bytes, Some("text/html"))
            .map_err(|e| BenchError::Index(format!("normalize {rel}: {e}")))?;
        let id = rel.trim_end_matches(".html").to_string();
        let source = id.split('/').next().unwrap_or("").to_string();
        let stem = id.rsplit('/').next().unwrap_or(&id).to_string();
        let title = nd.title.clone().unwrap_or(stem);
        docs.push(CorpusDoc {
            id,
            source,
            title,
            body: nd.text,
            url,
        });
    }
    Ok(docs)
}

// ---------------------------------------------------------------------------------------------
// Lanes
// ---------------------------------------------------------------------------------------------

/// Global BM25 over the Tantivy index. Returns the ranked doc ids.
fn bm25_lane(indexer: &Indexer, query: &str, k: usize) -> Result<Vec<String>, BenchError> {
    Ok(indexer
        .search(query, k)?
        .into_iter()
        .map(|r| r.doc_id)
        .collect())
}

/// Source aliases used by the routing lane. Pure lexical hints read from the query text; no
/// relevance label or query class is consulted.
fn source_aliases(source: &str) -> &'static [&'static str] {
    match source {
        "arxiv" => &["arxiv", "paper", "papers", "preprint"],
        "github" => &["github", "repo", "repository"],
        "docs" => &["docs", "documentation"],
        "community" => &["community", "forum", "reddit", "stackoverflow", "stack", "hacker"],
        "blog" => &["blog"],
        _ => &[],
    }
}

fn tokens(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Which sources a query explicitly names. Empty when the query carries no source hint.
pub fn route_sources(query: &str, sources: &[String]) -> Vec<String> {
    let toks = tokens(query);
    let mut out = Vec::new();
    for s in sources {
        let alias = source_aliases(s);
        if alias.iter().any(|a| toks.contains(*a)) {
            out.push(s.clone());
        }
    }
    out
}

/// Source-routed BM25 lane: scoped search for each matched source, merged; then (optionally)
/// the global BM25 search. Mirrors [`search_retrieval::SourceFirst`] but lets the caller drop
/// the global lane so `source-routing` can be measured with and without it.
fn routed_lane(
    searcher: &MemoryLaneSearcher,
    sources: &[String],
    query: &str,
    k: usize,
    with_global: bool,
) -> Result<Vec<String>, BenchError> {
    let mut raw = Vec::new();
    if !sources.is_empty() {
        let per = (k / sources.len()).max(1);
        for s in sources {
            raw.extend(
                searcher
                    .search(query, per, Some(s))
                    .map_err(|e| BenchError::Index(e.to_string()))?,
            );
        }
    }
    if with_global {
        raw.extend(
            searcher
                .search(query, k, None)
                .map_err(|e| BenchError::Index(e.to_string()))?,
        );
    }
    let merged = merge_dedup(raw);
    Ok(merged.into_iter().take(k).map(|c| c.doc_id).collect())
}

/// Minimal single-poll executor for the cross-encoder future (which is always ready).
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};
    let mut cx = Context::from_waker(Waker::noop());
    let mut fut = pin!(fut);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("cross-encoder future unexpectedly pending"),
    }
}

/// Rerank the first `depth` ids of `base` with the lexical cross-encoder over `text`, leaving
/// the tail in place. Deterministic: ties break by doc id ascending.
fn ce_rerank(query: &str, base: &[String], depth: usize, text: &HashMap<String, String>) -> Vec<String> {
    let ce = LexicalCrossEncoder;
    let head_len = depth.min(base.len());
    let (head, tail) = base.split_at(head_len);

    let mut scored: Vec<(String, f32)> = head
        .iter()
        .map(|id| {
            let doc = text.get(id).map(String::as_str).unwrap_or("");
            (id.clone(), block_on(ce.score(query, doc)))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut out: Vec<String> = scored.into_iter().map(|(id, _)| id).collect();
    out.extend(tail.iter().cloned());
    out
}

/// Resident set size from `/proc/self/statm` (field 2 = resident pages). 0 when unavailable.
pub fn peak_rss_kb() -> u64 {
    let Ok(text) = fs::read_to_string("/proc/self/statm") else {
        return 0;
    };
    let pages: u64 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let page = 4096u64; // conservative default; not read from the OS to stay dependency-free
    pages.saturating_mul(page) / 1024
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Rank of the first relevant doc (1-based, 0 = none found) in a ranked list.
fn first_relevant_rank(ranked: &[String], relevant: &[String]) -> usize {
    let rel: HashSet<&str> = relevant.iter().map(String::as_str).collect();
    for (i, d) in ranked.iter().enumerate() {
        if rel.contains(d.as_str()) {
            return i + 1;
        }
    }
    0
}

/// Per-case diagnostic: where each of the two headline lanes put the first relevant doc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaseRank {
    pub case_id: String,
    /// 1-based rank of the first relevant doc under plain bm25 (0 = none in top-k).
    pub bm25_rank: usize,
    /// 1-based rank under the full `bm25+dense+sparse` hybrid (0 = none in top-k).
    pub hybrid_rank: usize,
}

/// Metrics aggregator for one ablation across all cases.
struct Run {
    case_id: String,
    ranked: Vec<String>,
    latency_ms: u64,
    candidates: usize,
}

fn metrics_for(runs: &[Run], cases: &[QueryCase]) -> (Metrics, f64, usize) {
    let results: Vec<RunResult> = runs
        .iter()
        .map(|r| RunResult {
            case_id: r.case_id.clone(),
            ranked: r.ranked.clone(),
            latency_ms: r.latency_ms,
        })
        .collect();
    let pools: Vec<usize> = runs.iter().map(|r| r.candidates).collect();
    let mean = if pools.is_empty() {
        0.0
    } else {
        pools.iter().sum::<usize>() as f64 / pools.len() as f64
    };
    let max = pools.iter().copied().max().unwrap_or(0);
    (aggregate(&results, cases), mean, max)
}

// ---------------------------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------------------------

/// Build the index, run every ablation, and return the report (also written to `cfg.out_path`).
pub fn run(cfg: &Config) -> Result<Report, BenchError> {
    let cases = load_cases(&cfg.queries_path)?;
    if cases.is_empty() {
        return Err(BenchError::Eval("no query cases loaded".into()));
    }
    let docs = load_corpus(&cfg.corpus_dir)?;
    if docs.is_empty() {
        return Err(BenchError::Index("no corpus documents loaded".into()));
    }

    // --- build the Tantivy index from the fixture corpus ---------------------------------------
    let tmp = tempfile::Builder::new()
        .prefix("search-bench-")
        .tempdir()
        .map_err(|e| BenchError::Io(e.to_string()))?;
    let indexer = Indexer::open(IndexerConfig::new(tmp.path().join("tantivy")))?;
    indexer.index_batch(
        docs.iter()
            .map(|d| {
                IndexDoc::new(
                    d.id.clone(),
                    format!("res:{}", d.id),
                    d.url.clone(),
                    d.title.clone(),
                    d.body.clone(),
                )
                .with_source(d.source.clone())
            })
            .collect(),
    )?;
    indexer.publish_manifest()?;

    // --- dense lane data (selected embedder over title + body) ---------------------------------
    // The embedding dimension comes from the embedder itself (BGE is 384, hash defaults to 1024);
    // `cfg.dim` only feeds the hash embedder.
    let embedder = build_embedder(cfg)?;
    let embed_dim = embedder.dim();
    let doc_embs: Vec<(String, Embedding)> = docs
        .iter()
        .map(|d| {
            (
                d.id.clone(),
                embedder.embed(&format!("{} {}", d.title, d.body)),
            )
        })
        .collect();

    // --- source-first lane data (hermetic in-memory BM25 with source scoping) -------------------
    let mut mem = MemoryLaneSearcher::new();
    for d in &docs {
        mem.add_doc(
            MemoryDoc::new(d.id.clone(), d.title.clone())
                .with_body(d.body.clone())
                .with_source(d.source.clone())
                .with_url(d.url.clone()),
        );
    }

    let text_by_id: HashMap<String, String> = docs
        .iter()
        .map(|d| (d.id.clone(), format!("{} {}", d.title, d.body)))
        .collect();
    let mut sources: Vec<String> = docs.iter().map(|d| d.source.clone()).collect();
    sources.sort();
    sources.dedup();

    let rrf = Rrf::default();

    // Names in output order.
    const NAMES: [&str; 7] = [
        "bm25",
        "bm25+dense",
        "bm25+dense+sparse",
        "source-routing",
        "source-routing+global",
        "bm25+ce-on",
        "bm25+dense+sparse+ce-on",
    ];
    let mut runs: HashMap<&'static str, Vec<Run>> = HashMap::new();
    for n in NAMES {
        runs.insert(n, Vec::new());
    }
    let mut per_case: Vec<CaseRank> = Vec::with_capacity(cases.len());

    for case in &cases {
        let q = &case.query;

        // Lanes (each a ranked list of doc ids), timed.
        let t = Instant::now();
        let bm25 = bm25_lane(&indexer, q, cfg.k)?;
        let bm25_ms = t.elapsed().as_millis() as u64;

        let q_emb = embedder.embed(q);
        let t = Instant::now();
        let dense: Vec<String> = top_k(&q_emb, &doc_embs, cfg.k)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let dense_ms = t.elapsed().as_millis() as u64;

        let routed_sources = route_sources(q, &sources);
        let t = Instant::now();
        let routed = routed_lane(&mem, &routed_sources, q, cfg.k, false)?;
        let routed_ms = t.elapsed().as_millis() as u64;

        let t = Instant::now();
        let routed_global = routed_lane(&mem, &routed_sources, q, cfg.k, true)?;
        let routed_global_ms = t.elapsed().as_millis() as u64;

        // Fusions.
        let fused = |lanes: &[Vec<String>]| -> Vec<String> {
            rrf.fuse(lanes).into_iter().map(|(id, _)| id).collect()
        };
        let bd = fused(&[bm25.clone(), dense.clone()]);
        let bds = fused(&[bm25.clone(), dense.clone(), routed.clone()]);
        let sr_global = fused(&[routed.clone(), routed_global.clone()]);

        // Cross-encoder.
        let ce_bm25 = ce_rerank(q, &bm25, cfg.ce_depth, &text_by_id);
        let ce_bds = ce_rerank(q, &bds, cfg.ce_depth, &text_by_id);

        per_case.push(CaseRank {
            case_id: case.id.clone(),
            bm25_rank: first_relevant_rank(&bm25, &case.relevant),
            hybrid_rank: first_relevant_rank(&bds, &case.relevant),
        });

        let union_pool = |ls: &[&Vec<String>]| -> usize {
            let mut s: HashSet<&str> = HashSet::new();
            for l in ls {
                for id in l.iter() {
                    s.insert(id.as_str());
                }
            }
            s.len()
        };
        let pool = |l: &Vec<String>| l.len();

        let push = |runs: &mut HashMap<&'static str, Vec<Run>>,
                    name: &'static str,
                    ranked: Vec<String>,
                    candidates: usize,
                    latency_ms: u64| {
            runs.get_mut(name).unwrap().push(Run {
                case_id: case.id.clone(),
                ranked: ranked.into_iter().take(cfg.k).collect(),
                latency_ms,
                candidates,
            });
        };

        push(
            &mut runs,
            "bm25",
            bm25.clone(),
            pool(&bm25),
            bm25_ms,
        );
        push(
            &mut runs,
            "bm25+dense",
            bd.clone(),
            union_pool(&[&bm25, &dense]),
            bm25_ms + dense_ms + 1,
        );
        push(
            &mut runs,
            "bm25+dense+sparse",
            bds.clone(),
            union_pool(&[&bm25, &dense, &routed]),
            bm25_ms + dense_ms + routed_ms + 1,
        );
        push(
            &mut runs,
            "source-routing",
            routed.clone(),
            pool(&routed),
            routed_ms,
        );
        push(
            &mut runs,
            "source-routing+global",
            sr_global,
            union_pool(&[&routed, &routed_global]),
            routed_global_ms,
        );
        push(
            &mut runs,
            "bm25+ce-on",
            ce_bm25,
            pool(&bm25),
            bm25_ms + cfg.ce_depth.min(bm25.len()) as u64 + 1,
        );
        push(
            &mut runs,
            "bm25+dense+sparse+ce-on",
            ce_bds,
            union_pool(&[&bm25, &dense, &routed]),
            bm25_ms + dense_ms + routed_ms + cfg.ce_depth.min(bds.len()) as u64 + 2,
        );
    }

    // --- metrics table -------------------------------------------------------------------------
    let mut ablations = Vec::new();
    let mut budget_mean = 0.0f64;
    let mut budget_max = 0usize;
    for name in NAMES {
        let rs = runs.remove(name).unwrap();
        let (metrics, mean, max) = metrics_for(&rs, &cases);
        budget_mean += mean;
        budget_max = budget_max.max(max);
        ablations.push(AblationRow {
            name: name.to_string(),
            note: ablation_note(name, cfg.embedder.short_name()),
            metrics,
            mean_candidates: mean,
            max_candidates: max,
        });
    }
    let n_abl = ablations.len() as f64;
    budget_mean /= n_abl.max(1.0);

    let mut notes = vec![
        "SMOKE SET, NOT A RESEARCH CLAIM: 30-doc fixture corpus + 25 hand-labelled queries."
            .to_string(),
    ];
    match cfg.embedder {
        EmbedderKind::Hash => {
            notes.push(
                "Dense lane: HashEmbedder (deterministic hashed bag-of-words), NOT a semantic model."
                    .to_string(),
            );
            #[cfg(feature = "bge")]
            notes.push(
                "The semantic lane is available in this binary: rerun with `--embedder bge` to \
                 measure BGE-small-en-v1.5 (384-d)."
                    .to_string(),
            );
            #[cfg(not(feature = "bge"))]
            notes.push(
                "UNAVAILABLE: real BGE-small weights (feature `bge`) are not compiled into this \
                 binary; rebuild with `--features bge -- --embedder bge` to measure the semantic lane."
                    .to_string(),
            );
        }
        EmbedderKind::Bge => {
            notes.push(
                "Dense lane: BGE-small-en-v1.5 (384-d, CLS-pooled, L2-normalized) — a semantic model."
                    .to_string(),
            );
        }
    }
    notes.push(
        "UNAVAILABLE: a trained cross-encoder; the LexicalCrossEncoder (deterministic lexical \
         overlap) is used as an honest proxy."
            .to_string(),
    );
    notes.push(
        "Source routing uses lexical source hints in the query text (e.g. \"github\", \"reddit\", \
         \"docs\") and never reads class or relevance labels."
            .to_string(),
    );

    let budget = Budget {
        docs: docs.len(),
        queries: cases.len(),
        k: cfg.k,
        ce_depth: cfg.ce_depth,
        embed_dim,
        embedder: cfg.embedder.display_name().to_string(),
        peak_rss_kb: peak_rss_kb(),
        mean_candidates: budget_mean,
        max_candidates: budget_max,
    };

    let report = Report {
        version: env!("CARGO_PKG_VERSION").to_string(),
        generated_unix: now_unix(),
        corpus_dir: cfg.corpus_dir.display().to_string(),
        queries_path: cfg.queries_path.display().to_string(),
        budget,
        notes,
        ablations,
        per_case,
    };

    if let Some(parent) = cfg.out_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&cfg.out_path, serde_json::to_vec_pretty(&report)?)?;

    Ok(report)
}

fn ablation_note(name: &str, embedder_short: &str) -> String {
    match name {
        "bm25" => "global lexical BM25 over the Tantivy index (base lane)".into(),
        "bm25+dense" => format!("RRF(bm25, {embedder_short} top-k over title+body)"),
        "bm25+dense+sparse" => "RRF(bm25, dense, source-routed BM25 lane)".into(),
        "source-routing" => "source-routed BM25 only; no global lane (empty when no source hint)".into(),
        "source-routing+global" => "RRF(source-routed, global BM25)".into(),
        "bm25+ce-on" => "bm25 top-50 reranked by LexicalCrossEncoder (ce-off == bm25 row)".into(),
        "bm25+dense+sparse+ce-on" => {
            "full hybrid top-50 reranked by LexicalCrossEncoder (ce-off == bm25+dense+sparse row)".into()
        }
        _ => String::new(),
    }
}

/// Render the metrics table exactly as the `bench` binary prints it.
pub fn render_table(report: &Report) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "{:<26} {:>9} {:>8} {:>10} {:>8} {:>8} {:>11}\n",
        "ablation", "nDCG@10", "MRR", "recall@10", "p50 ms", "p95 ms", "candidates"
    ));
    s.push_str(&"-".repeat(84));
    s.push('\n');
    for a in &report.ablations {
        s.push_str(&format!(
            "{:<26} {:>9.4} {:>8.4} {:>10.4} {:>8} {:>8} {:>11.1}\n",
            a.name,
            a.metrics.ndcg_at_10,
            a.metrics.mrr,
            a.metrics.recall_at_10,
            a.metrics.p50_ms,
            a.metrics.p95_ms,
            a.mean_candidates
        ));
    }
    s
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_loads_from_default_path() {
        let docs = load_corpus(Path::new(DEFAULT_CORPUS_DIR)).unwrap();
        assert!(docs.len() >= 20, "expected >=20 docs, got {}", docs.len());
        assert!(docs.iter().all(|d| !d.title.is_empty()));
        assert!(docs.iter().all(|d| !d.body.is_empty()));
        let sources: HashSet<&str> = docs.iter().map(|d| d.source.as_str()).collect();
        assert!(sources.len() >= 3, "expected several sources, got {sources:?}");
    }

    #[test]
    fn routing_is_lexical_and_label_free() {
        let sources: Vec<String> = vec![
            "arxiv".into(),
            "github".into(),
            "docs".into(),
            "community".into(),
            "blog".into(),
        ];
        assert_eq!(
            route_sources("github repository tokio", &sources),
            vec!["github".to_string()]
        );
        assert!(route_sources("scaled dot product attention", &sources).is_empty());
    }

    #[test]
    fn rss_probe_is_nonzero_on_linux() {
        // /proc/self/statm exists on the CI Linux host; the value must be plausible.
        assert!(peak_rss_kb() > 0);
    }

    #[test]
    fn default_embedder_is_hash_and_labels_say_lexical_not_semantic() {
        // Default behaviour must be unchanged: HashEmbedder.
        let cfg = Config::default();
        assert_eq!(cfg.embedder, EmbedderKind::Hash);
        assert_eq!(EmbedderKind::default(), EmbedderKind::Hash);
        assert_eq!(cfg.embedder.as_str(), "hash");

        // The lane label and the report's display name must name the hash embedder...
        let note = ablation_note("bm25+dense", cfg.embedder.short_name());
        assert!(note.contains("HashEmbedder"), "lane note was: {note}");

        let disp = cfg.embedder.display_name();
        assert!(disp.contains("HashEmbedder"), "display name was: {disp}");
        // ...and must state that it is lexical and NOT a semantic model.
        assert!(disp.contains("lexical"), "display name was: {disp}");
        assert!(
            disp.contains("NOT a semantic model"),
            "display name was: {disp}"
        );
        // It must never claim to be a semantic/real model.
        assert!(!disp.contains("BGE"), "hash name must not claim BGE: {disp}");
        assert!(
            !disp.contains("; semantic)"),
            "hash name must not claim to be semantic: {disp}"
        );
    }

    #[test]
    fn embedder_selector_parses_hash_and_bge() {
        assert_eq!(EmbedderKind::parse("hash").unwrap(), EmbedderKind::Hash);
        assert_eq!(EmbedderKind::parse("bge").unwrap(), EmbedderKind::Bge);
        assert_eq!(EmbedderKind::parse("BGE").unwrap(), EmbedderKind::Bge);
        assert!("hge".parse::<EmbedderKind>().is_err());
        assert!(EmbedderKind::parse("").is_err());
    }

    #[test]
    fn hash_embedder_build_matches_dim() {
        let cfg = Config {
            dim: 256,
            ..Config::default()
        };
        let e = build_embedder(&cfg).expect("hash builds");
        assert_eq!(e.dim(), 256);
    }

    /// In a build without `feature = "bge"`, selecting the BGE embedder must fail loudly with an
    /// actionable rebuild command — never silently fall back to hash.
    #[cfg(not(feature = "bge"))]
    #[test]
    fn bge_without_feature_errors_actionably() {
        let cfg = Config {
            embedder: EmbedderKind::Bge,
            ..Config::default()
        };
        let err = build_embedder(&cfg).map(|_| ()).unwrap_err().to_string();
        assert!(err.contains("--features bge"), "error was: {err}");
        assert!(err.contains("--embedder bge"), "error was: {err}");
    }

    /// With `feature = "bge"`, the labels name BGE and the dimension is 384. Ignored by default:
    /// it constructs the real model, which may require a weights download.
    #[cfg(feature = "bge")]
    #[test]
    #[ignore = "constructs the real BGE-small model (may need a weights download)"]
    fn bge_labels_name_bge_and_dim_is_384() {
        let cfg = Config {
            embedder: EmbedderKind::Bge,
            ..Config::default()
        };
        let disp = cfg.embedder.display_name();
        assert!(disp.contains("BGE-small-en-v1.5"), "display name was: {disp}");
        assert!(disp.contains("384-d"), "display name was: {disp}");
        assert!(disp.contains("CLS-pooled"), "display name was: {disp}");
        assert!(disp.contains("L2-normalized"), "display name was: {disp}");
        let note = ablation_note("bm25+dense", cfg.embedder.short_name());
        assert!(note.contains("BGE-small-en-v1.5"), "lane note was: {note}");
        let e = build_embedder(&cfg).expect("bge embedder builds");
        assert_eq!(e.dim(), 384);
        assert_eq!(e.dim(), search_embed::bge::BGE_SMALL_DIM);
    }
}
