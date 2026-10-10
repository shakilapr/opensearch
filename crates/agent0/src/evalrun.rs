//! Run a labelled query set through the **shipped engine** and report the metrics.
//!
//! `agent0 eval build-set` produces the cases; this runs them. The path is the same one the HTTP
//! server uses — the same searcher, the same extra lanes (dense and §9.4 graph), the same
//! `run_engine_search` — called in-process, so a number reported here describes what a request
//! would actually get rather than a private benchmark harness. The answer generator is deliberately
//! left off: this measures *ranking*, and a generated answer would neither change nor explain it.
//!
//! Honest limits: the labels are corpus-derived (see `evalset`), so the number is "nDCG of finding
//! the page a title describes", not human-judged relevance; and the run is single-box with whatever
//! lanes are built on this host, which the output records.

use std::path::PathBuf;

use search_core::contract::SearchRequest;
use search_eval::{aggregate, load_cases, RunResult};

use crate::{build_dense_lane, build_graph_lane};

pub struct Options {
    set: PathBuf,
    limit: usize,
    out: Option<PathBuf>,
    /// Which injected lanes to run: `all` (default), `none`, `dense`, `graph`. This is the
    /// experiment matrix the plan needs — an uncached request's cost must be attributed to a lane
    /// before any lane is optimised or routed away from.
    lanes: String,
    /// Opt into the §6–§18 engine (false = the frozen legacy lexical path).
    engine: bool,
}

impl Options {
    fn from_args(args: &[String]) -> Self {
        let mut opts = Options {
            set: PathBuf::from("benchmarks/queries.corpus.jsonl"),
            limit: 10,
            out: None,
            lanes: "all".to_string(),
            engine: true,
        };
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--set" => {
                    if let Some(v) = args.get(i + 1) {
                        opts.set = PathBuf::from(v);
                    }
                    i += 2;
                }
                "--limit" => {
                    if let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) {
                        opts.limit = v;
                    }
                    i += 2;
                }
                "--out" => {
                    if let Some(v) = args.get(i + 1) {
                        opts.out = Some(PathBuf::from(v));
                    }
                    i += 2;
                }
                "--lanes" => {
                    if let Some(v) = args.get(i + 1) {
                        opts.lanes = v.to_ascii_lowercase();
                    }
                    i += 2;
                }
                "--no-engine" => {
                    opts.engine = false;
                    i += 1;
                }
                _ => i += 1,
            }
        }
        opts
    }
}

pub async fn run(args: &[String]) -> std::process::ExitCode {
    let opts = Options::from_args(args);
    let cfg = search_core::config::Config::from_env();

    // Resolve a relative path against the workspace, not the caller's idea of cwd.
    let set_path = if opts.set.is_absolute() {
        opts.set.clone()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(&opts.set)
    };
    let cases = match load_cases(&set_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to load {}: {e}", set_path.display());
            return std::process::ExitCode::from(1);
        }
    };
    if cases.is_empty() {
        eprintln!("{} contains no cases", set_path.display());
        return std::process::ExitCode::from(1);
    }

    // Read-only: measuring must not take the writer lock from a running server or crawl.
    let pipeline = match super::Pipeline::open_read_only(&cfg, false) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to open the index: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    let searcher = pipeline.searcher();
    let mut extra_lanes = Vec::new();
    if matches!(opts.lanes.as_str(), "all" | "dense") {
        extra_lanes.extend(build_dense_lane(&cfg));
    }
    if matches!(opts.lanes.as_str(), "all" | "graph") {
        extra_lanes.extend(build_graph_lane(&cfg));
    }
    eprintln!(
        "lanes: {} injected beside the index-backed lanes (--lanes {})",
        extra_lanes.len(),
        opts.lanes
    );

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let mut results: Vec<RunResult> = Vec::with_capacity(cases.len());
    let mut failures = 0usize;
    for case in &cases {
        let req = search_api::engine::EngineSearchRequest {
            base: SearchRequest {
                query: case.query.clone(),
                limit: opts.limit,
                offset: 0,
                timezone: "UTC".to_string(),
                deadline_ms: None,
                filters: None,
            },
            engine: opts.engine,
            mode: search_api::engine::EngineMode::Auto,
        };
        let started = std::time::Instant::now();
        match search_api::engine::run_engine_search(
            searcher.as_ref(),
            req,
            now_ms,
            None,
            None,
            &extra_lanes,
        ) {
            Ok(resp) => {
                let ranked: Vec<String> =
                    resp.results.iter().map(|r| r.base.doc_id.clone()).collect();
                results.push(RunResult {
                    case_id: case.id.clone(),
                    ranked,
                    latency_ms: started.elapsed().as_millis() as u64,
                });
            }
            Err(e) => {
                failures += 1;
                if failures <= 3 {
                    eprintln!("case {} failed: {e}", case.id);
                }
            }
        }
    }

    // Cases that failed to run are scored as empty rankings rather than dropped: dropping them
    // would silently raise the average by removing the queries the engine could not serve.
    for case in &cases {
        if !results.iter().any(|r| r.case_id == case.id) {
            results.push(RunResult { case_id: case.id.clone(), ranked: Vec::new(), latency_ms: 0 });
        }
    }

    let metrics = aggregate(&results, &cases);
    println!("eval run: {} case(s) from {}", cases.len(), set_path.display());
    println!("  nDCG@10      : {:.4}", metrics.ndcg_at_10);
    println!("  MRR          : {:.4}", metrics.mrr);
    println!("  recall@10    : {:.4}", metrics.recall_at_10);
    println!("  latency p50  : {} ms", metrics.p50_ms);
    println!("  latency p95  : {} ms", metrics.p95_ms);
    println!("  lane errors  : {failures}");
    println!("  config       : lanes={} engine={}", opts.lanes, opts.engine);
    println!("  labels       : corpus-derived (title → its document), not human judgements");

    if let Some(out) = &opts.out {
        match serde_json::to_string_pretty(&metrics) {
            Ok(body) => {
                if let Err(e) = std::fs::write(out, body) {
                    eprintln!("failed to write {}: {e}", out.display());
                    return std::process::ExitCode::from(1);
                }
                println!("  metrics json : {}", out.display());
            }
            Err(e) => {
                eprintln!("failed to serialise metrics: {e}");
                return std::process::ExitCode::from(1);
            }
        }
    }

    std::process::ExitCode::SUCCESS
}
