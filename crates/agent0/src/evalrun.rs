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

use std::collections::BTreeMap;
use std::path::PathBuf;

use search_core::contract::SearchRequest;
use search_eval::{aggregate, load_cases, RunResult};

use crate::{build_concept_lane, build_dense_lane, build_graph_lane};

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
    /// Search mode. `Fast` by default because the benchmark must be hermetic and repeatable;
    /// `Auto`/`Deep` perform live fetches, which a measurement run cannot afford.
    mode: search_api::engine::EngineMode,
}

impl Options {
    fn from_args(args: &[String]) -> Self {
        let mut opts = Options {
            set: PathBuf::from("benchmarks/queries.corpus.jsonl"),
            limit: 10,
            out: None,
            lanes: "all".to_string(),
            engine: true,
            mode: search_api::engine::EngineMode::Fast,
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
                "--mode" => {
                    let v = args.get(i + 1).map(String::as_str).unwrap_or("fast");
                    opts.mode = match v {
                        "auto" => search_api::engine::EngineMode::Auto,
                        "deep" => search_api::engine::EngineMode::Deep,
                        "fast" => search_api::engine::EngineMode::Fast,
                        other => {
                            // Fall back rather than exit: this parser returns Options, and a
                            // benchmark that refuses to start over a typo wastes a full run.
                            eprintln!("unknown --mode `{other}` (fast|auto|deep); using fast");
                            search_api::engine::EngineMode::Fast
                        }
                    };
                    i += 2;
                }
                _ => i += 1,
            }
        }
        opts
    }
}

/// Nearest-rank percentile over `values` (sorts in place). Empty → 0. Mirrors
/// `search_eval`'s latency percentile so the stage numbers use the same convention.
fn percentile_micros(values: &mut [u64], p: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let n = values.len();
    let rank = (p / 100.0 * n as f64).ceil() as usize;
    values[rank.saturating_sub(1).min(n - 1)]
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
    // The concept lane reads `doc_concept`, the weighted matrix v2 §6 specifies. Measuring it is
    // the whole point of `--lanes concept`: the lane existed and was tested for a day before
    // anything served it, so a configuration that cannot be measured is a configuration that
    // silently does nothing.
    if matches!(opts.lanes.as_str(), "all" | "concept") {
        extra_lanes.extend(build_concept_lane(&cfg));
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

    // Diagnostic per-stage timing for this run only. `timing::enable` is a no-op for the
    // request path's determinism (see `search_engine::timing`); it just makes the engine
    // record `Instant` deltas that we drain per case. Off, the engine reads no clock.
    search_engine::timing::enable();
    let mut stage_us: BTreeMap<&'static str, Vec<u64>> = BTreeMap::new();
    let mut stage_count: BTreeMap<&'static str, Vec<u64>> = BTreeMap::new();
    // Diagnostic scalars the API records per request: the candidate limit actually passed to
    // `Searcher::search` and the base segment count. This is the attribution line.
    let mut stage_gauge: BTreeMap<&'static str, Vec<u64>> = BTreeMap::new();

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
            // Fast by default: `Auto` performs §17's 8 live fetches per query, and a benchmark
            // run inside this process has no network path — each fetch waits out its timeout, so
            // 3 queries took over 280 s and reported nothing. Fast mode's refresh_count is 0, which
            // is what makes a 604-case run tractable and repeatable. `--mode deep` opts back in.
            mode: opts.mode,
            // A benchmark measures retrieval. Asking for an answer would put a model call inside
            // every case and measure the provider, not the engine.
            answer: false,
        };
        let started = std::time::Instant::now();
        let _ = search_engine::timing::take();
        let _ = search_engine::timing::take_counts();
        let _ = search_api::engine::take_gauges();
        let ranked: Result<Vec<String>, String> = if opts.engine {
            search_api::engine::run_engine_search(
                searcher.as_ref(),
                req,
                now_ms,
                None,
                None,
                &extra_lanes,
            )
            .map(|resp| resp.results.iter().map(|r| r.base.doc_id.clone()).collect())
            .map_err(|e| e.to_string())
        } else {
            // `--no-engine`: the frozen legacy lexical path, exactly as the server runs it
            // when `engine: false` (api/src/server.rs), so the comparison is real rather
            // than a second copy of the engine path.
            search_api::query::execute(searcher.as_ref(), &req.base)
                .map(|resp| resp.results.iter().map(|r| r.doc_id.clone()).collect())
                .map_err(|e| e.to_string())
        };
        match ranked {
            Ok(ranked) => {
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
        // Sum the stages a single case ran (retrieval/evidence fire once per round) and
        // append one observation per stage, so percentiles are per-query totals.
        let mut per_case: BTreeMap<&'static str, u64> = BTreeMap::new();
        for (stage, us) in search_engine::timing::take() {
            *per_case.entry(stage).or_default() += us;
        }
        for (stage, us) in per_case {
            stage_us.entry(stage).or_default().push(us);
        }
        let mut per_case_counts: BTreeMap<&'static str, u64> = BTreeMap::new();
        for (stage, n) in search_engine::timing::take_counts() {
            *per_case_counts.entry(stage).or_default() += n;
        }
        for (stage, n) in per_case_counts {
            stage_count.entry(stage).or_default().push(n);
        }
        for (name, value) in search_api::engine::take_gauges() {
            stage_gauge.entry(name).or_default().push(value);
        }
    }
    search_engine::timing::disable();

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
    // Per-stage breakdown over the cases actually run. Values are per-query totals (a stage
    // that ran in several rounds is summed), reported at p50/p95 in milliseconds.
    if !stage_us.is_empty() {
        println!("  per-stage (ms, per-query totals):");
        for (stage, mut v) in stage_us {
            let p50 = percentile_micros(&mut v, 50.0) as f64 / 1000.0;
            let p95 = percentile_micros(&mut v, 95.0) as f64 / 1000.0;
            println!("    {stage:<16} p50 {p50:8.1}  p95 {p95:8.1}  n={}", v.len());
        }
    }
    if !stage_count.is_empty() {
        println!("  per-stage call counts (per query):");
        for (stage, mut v) in stage_count {
            let p50 = percentile_micros(&mut v, 50.0);
            let p95 = percentile_micros(&mut v, 95.0);
            println!("    {stage:<16} p50 {p50:8}  p95 {p95:8}  n={}", v.len());
        }
    }
    // The attribution line: the candidate limit the scan actually ran with, and the index
    // layout it ran against. `index_scan_limit` is 10 for the legacy path; for the engine path
    // it is the first-pass per-lane breadth, now sized to the requested limit (see
    // `search_engine::plan::size_first_pass_top_k`) rather than the mode ceiling of up to 200.
    if !stage_gauge.is_empty() {
        println!("  scan config (per query):");
        for (name, mut v) in stage_gauge {
            let p50 = percentile_micros(&mut v, 50.0);
            let p95 = percentile_micros(&mut v, 95.0);
            println!("    {name:<16} p50 {p50:8}  p95 {p95:8}  n={}", v.len());
        }
    }
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
