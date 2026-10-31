//! `agent0` — Stage-0 command line.
//!
//! ```text
//! agent0 ingest <url>            one URL through the full pipeline
//! agent0 search <query> [limit]  query the live index
//! agent0 serve                   start the axum /search API over the index
//! agent0 refresh                 one Stage-3 background refresh tick
//! agent0 seeds arxiv [...]       arXiv Atom feed -> a seed file
//! agent0 seeds oai [...]         OAI-PMH ListRecords URLs -> a seed file
//! agent0 export <dir> [...]      the live index -> a portable dataset pack
//! agent0 import <dir> [limit]    a portable dataset pack -> the live index
//! ```
//!
//! `export`/`import` are the two halves of the portable knowledge model: export
//! compiles what this machine has scraped into a pack (optionally structured by
//! the cheap LLM), import loads that pack into an index on any other machine.
//!
//! Configuration comes from the environment via `search_core::Config::from_env`
//! (data dir, bind address, limits, user agent).

use std::process::ExitCode;

use search_agent0::Pipeline;
use search_core::config::Config;

/// Open the pipeline for serving, honouring `SEARCH_READ_ONLY`.
///
/// The crawler holds the index writer lock for the whole of a run, so a server that insists on
/// its own writer cannot come up beside one — which is why serving and crawling used to be
/// mutually exclusive. `auto` (the default) tries the writer, and on a locked index falls back
/// to a read-only open: queries work, query-time refresh does not, and the log says so.
///
/// * `SEARCH_READ_ONLY=1` — always read-only (a crawl may be running; never fails on the lock).
/// * `SEARCH_READ_ONLY=0` — always take the writer; fail if the index is locked.
/// * unset / `auto` — writer if possible, read-only otherwise.
mod evalrun;


/// Build the dense lane over the vector sidecar, when one exists.
///
/// The binary owns this choice: it knows the data root, the `SEARCH_MODEL_CACHE` location and
/// whether this build has the `bge` feature. An absent sidecar is a normal state (the corpus has
/// not been backfilled) and yields no lane; a *mismatched* one is logged and skipped rather than
/// allowed to inject noise into every query.
fn build_dense_lane(
    cfg: &search_core::config::Config,
) -> Vec<std::sync::Arc<dyn search_retrieval::LaneSearcher + Send + Sync>> {
    // Every root that is served, not just the primary. Measured 2026-10-31: the fleet's four
    // shards had 2.0 GB of vectors (`agent0 embed build` finished at 02:30) while the dense lane
    // still read only `/var/lib/search/vectors.bin`, covering 19,255 of 1.35M documents — 1.4% of
    // the corpus had a vector that any query could reach.
    let mut dirs = vec![cfg.data_dir.clone()];
    // Fleet vectors are opt-in. Measured 2026-10-31 with all five roots enabled: p50 **6.5 s** and
    // p95 12.5 s per query, against 184 ms before. The lane is a brute-force cosine scan — measured
    // 1.07 µs per document, so 1.39M documents is ~1.5 s for ONE pass, and the engine runs a lane
    // once per query variant across five stores.
    //
    // The vectors are correct and now complete (1,387,749 documents); what is missing is the
    // approximate index that makes them affordable at this size — v2 §9/§10, not yet built. Until
    // then the default serves the 19K-document primary root, which costs 28 ms.
    if std::env::var("SEARCH_DENSE_FLEET").map(|v| v != "0" && !v.is_empty()).unwrap_or(false) {
        dirs.extend(search_agent0::fedserve::dirs_from_env(&cfg.data_dir));
    } else {
        tracing::info!(
            "dense lane: primary root only (set SEARCH_DENSE_FLEET=1 to scan fleet vectors; \
             measured 6.5 s p50 for 1.39M documents without an approximate index)"
        );
    }

    // Built once: the BGE weights are ~130 MB and loading them per shard would cost seconds and
    // four times the memory for an identical model.
    let embedder: Option<std::sync::Arc<dyn search_embed::Embedder>> = {
        #[cfg(feature = "bge")]
        {
            let cache = std::env::var("SEARCH_MODEL_CACHE")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| cfg.data_dir.join("fastembed"));
            match search_embed::bge::BgeEmbedder::with_cache_dir(cache) {
                Ok(e) => Some(std::sync::Arc::new(e)),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "BGE weights unavailable: serving without the dense lane"
                    );
                    None
                }
            }
        }
        #[cfg(not(feature = "bge"))]
        {
            Some(std::sync::Arc::new(search_embed::HashEmbedder::new(256)))
        }
    };
    let Some(embedder) = embedder else {
        return Vec::new();
    };

    let mut lanes: Vec<std::sync::Arc<dyn search_retrieval::LaneSearcher + Send + Sync>> =
        Vec::new();
    let mut skipped = 0usize;
    for dir in &dirs {
        let path = dir.join("vectors.bin");
        if !path.exists() {
            skipped += 1;
            continue;
        }
        match search_retrieval::DenseLane::from_path(&path, std::sync::Arc::clone(&embedder)) {
            Ok(Some(lane)) => {
                tracing::info!(
                    path = %path.display(),
                    docs = lane.len(),
                    "dense lane enabled"
                );
                lanes.push(std::sync::Arc::new(lane));
            }
            Ok(None) => {
                skipped += 1;
            }
            Err(e) => {
                // One unreadable store must not disable the others: a partially embedded fleet
                // should serve the vectors it has.
                tracing::warn!(path = %path.display(), error = %e, "vector store unusable; skipping");
                skipped += 1;
            }
        }
    }
    if lanes.is_empty() {
        tracing::info!(
            dirs = dirs.len(),
            "no vector stores: the dense lane is off (build one with `agent0 embed build`)"
        );
    } else {
        tracing::info!(lanes = lanes.len(), skipped, "dense lanes enabled");
    }
    lanes
}

/// Build the §9.4 graph lane over the sidecar `agent0 graph build` writes.
///
/// Absent sidecar = no lane (the graph has not been built): a normal state, logged at info. A
/// present-but-unreadable sidecar is skipped with a warning rather than failing the server, because
/// a serving API that refuses to start is worse than one without graph expansion.
fn build_graph_lane(
    cfg: &search_core::config::Config,
) -> Vec<std::sync::Arc<dyn search_retrieval::LaneSearcher + Send + Sync>> {
    let path = cfg.graph_sidecar();
    if !path.exists() {
        tracing::info!(
            path = %path.display(),
            "no graph sidecar: the graph lane is off (build one with `agent0 graph build`)"
        );
        return Vec::new();
    }
    match search_api::graph_lane::GraphLaneSearcher::from_path(&path) {
        Ok(lane) => {
            tracing::info!(
                path = %path.display(),
                entities = lane.entity_count(),
                "graph lane enabled"
            );
            vec![std::sync::Arc::new(lane)]
        }
        Err(e) => {
            tracing::warn!(error = %e, "graph sidecar unusable: serving without the graph lane");
            Vec::new()
        }
    }
}

/// Build the concept lane over the `doc_concept` matrix.
///
/// This is the lane `docs/SEMANTIC_INDEX_ARCHITECTURE.md` describes and `architecture-update-v2.md`
/// §6 requires, and until 2026-10-29 it existed but nothing in the serving path referenced it: the
/// live API answered "chemical reactions that affect romance" with the band My Chemical Romance.
///
/// No associations = no lane, logged at info. An empty lane would take a share of every fused
/// result's weight and contribute nothing, which is worse than being absent.
///
/// The lane reads each shard's own `doc_concept` table. Federation fuses the shards after retrieval,
/// so a document's id is the same on every shard and needs no rewriting here.
pub(crate) fn build_concept_lane(
    cfg: &search_core::config::Config,
) -> Vec<std::sync::Arc<dyn search_retrieval::LaneSearcher + Send + Sync>> {
    // Every root that is actually served, not just the primary: the matrix was built on the fleet,
    // and a lane over the primary alone found no associations at all.
    let mut dirs = vec![cfg.data_dir.clone()];
    dirs.extend(search_agent0::fedserve::dirs_from_env(&cfg.data_dir));
    match search_retrieval::concept::FederatedConceptLane::from_dirs(&dirs) {
        Some(lane) => {
            tracing::info!(
                shards = lane.shard_count(),
                of = dirs.len(),
                "concept lane enabled (doc_concept matrix); measured 2026-10-29: at RRF c=60 a                  lane's contribution is 1/(60+rank), so a document found by the concept lane alone                  (0.0138 at rank 1) cannot outrank one found by both lexical lanes (0.0303). The                  lane therefore re-ranks rather than retrieves until c is lowered or the score is                  weighted, which is what Phase D of EXECUTION-PLAN-v2.md must measure"
            );
            vec![std::sync::Arc::new(lane)]
        }
        None => {
            tracing::info!(
                dirs = dirs.len(),
                "no concept associations: the concept lane is off (build with `agent0 concepts build`)"
            );
            Vec::new()
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // Honour RUST_LOG (default `info`); without this the deploy unit's RUST_LOG
    // produces no output at all.
    search_agent0::init_tracing();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("ingest") => match args.get(2) {
            Some(url) => run_ingest(url).await,
            None => usage("agent0 ingest <url>"),
        },
        Some("search") => match args.get(2) {
            Some(q) => {
                let limit = args
                    .get(3)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(10);
                run_search(q, limit).await
            }
            None => usage("agent0 search <query> [limit]"),
        },
        Some("migrate") => search_agent0::migrate::run(&args),
        Some("concepts") => match args.get(2).map(String::as_str) {
            Some("build") => search_agent0::conceptbuild::run(&args).await,
            _ => {
                eprintln!("usage: agent0 concepts build [--dir PATH]... [--batch N] [--limit N] [--dry-run]");
                ExitCode::from(2)
            }
        },
        Some("index") => match args.get(2).map(String::as_str) {
            Some("build") => search_agent0::indexbuild::run(&args).await,
            _ => {
                eprintln!("usage: agent0 index build [--dir PATH]... [--batch N] [--limit N]");
                ExitCode::from(2)
            }
        },
        Some("serve") => run_serve().await,
        Some("crawl") => search_agent0::crawl::run_crawl(&args).await,
        Some("eval") => match args.get(2).map(String::as_str) {
            Some("build-set") => search_agent0::evalset::run(&args).await,
            Some("run") => evalrun::run(&args).await,
            _ => {
                eprintln!("usage: agent0 eval build-set [--out FILE] [--sample N] [--min-title-words N] [--max-title-words N] [--min-body-chars N]");
                eprintln!("       agent0 eval run [--set FILE] [--limit N] [--out metrics.json] [--lanes all|none|dense|graph] [--no-engine]");
                ExitCode::from(2)
            }
        },
        Some("graph") => match args.get(2).map(String::as_str) {
            Some("build") => search_agent0::graphbuild::run(&args).await,
            _ => {
                eprintln!("usage: agent0 graph build [--limit N] [--min-count C] [--min-docs D] [--window W]");
                ExitCode::from(2)
            }
        },
        Some("frontier") => match args.get(2).map(String::as_str) {
            Some("recover") => search_agent0::frontier_recover::run(&args),
            _ => {
                eprintln!("usage: agent0 frontier recover [--dir PATH]... [--now EPOCH]");
                ExitCode::from(2)
            }
        },
        Some("embed") => match args.get(2).map(String::as_str) {
            Some("build") => search_agent0::embedbuild::run(&args).await,
            _ => {
                eprintln!(
                    "usage: agent0 embed build [--out PATH] [--limit N] [--max-chars N] [--embedder bge|hash]"
                );
                ExitCode::from(2)
            }
        },
        Some("refresh") => run_refresh().await,
        Some("export") => run_export(&args).await,
        Some("import") => run_import(&args).await,
        Some("seeds") => search_agent0::seeds::run(&args).await,
        _ => usage(
            "agent0 <ingest <url> | search <query> [limit] | refresh | serve \
             | crawl <seedsfile> [--max-docs N] [--concurrency C] [--out DIR] [--max-urls N] \
             | seeds arxiv [--query Q] [--max N] [--out FILE] \
             | seeds oai [--repo NAME=URL[#SET]]... [--from D] [--until D] [--out FILE] \
             | frontier recover [--dir PATH]... \
             | export <dir> [--limit N] [--raw] [--name NAME] | import <dir> [--limit N]>",
        ),
    }
}

fn usage(msg: &str) -> ExitCode {
    eprintln!("usage: {msg}");
    ExitCode::from(2)
}

fn config() -> Config {
    Config::from_env()
}

/// `--flag N`, parsed; `None` when absent or unparseable.
fn flag_usize(args: &[String], flag: &str) -> Option<usize> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse::<usize>().ok())
}

/// `--flag VALUE`; `None` when absent or empty.
fn flag_str(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .filter(|v| !v.trim().is_empty())
        .cloned()
}

async fn run_ingest(url: &str) -> ExitCode {
    let cfg = config();
    let pipeline = match Pipeline::open(&cfg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to open pipeline: {e}");
            return ExitCode::from(1);
        }
    };
    match pipeline.crawl_one(url).await {
        Ok(outcome) => {
            println!("{}", serde_json::to_string_pretty(&outcome_json(&outcome)).unwrap());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ingest failed: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run_search(query: &str, limit: usize) -> ExitCode {
    let cfg = config();
    // Honour `SEARCH_READ_ONLY`. A running server (or a crawl) holds the index writer, so a
    // query must still be answerable beside it: before this, `agent0 search` failed outright
    // with `Failed to acquire Lockfile: LockBusy` while `agent0 serve` was up.
    let pipeline = match search_agent0::open_honouring_read_only(&cfg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to open pipeline: {e}");
            return ExitCode::from(1);
        }
    };
    // Opt-in live refresh: `SEARCH_QUERY_REFRESH=1 agent0 search "latest X"` runs
    // a P0 query-triggered refresh before searching. Off by default (hermetic).
    let fresh = matches!(
        std::env::var("SEARCH_QUERY_REFRESH").as_deref(),
        Ok("1") | Ok("true") | Ok("on")
    );
    // A read-only pipeline cannot write the index, so the refresh would be a silent no-op.
    // Do not let a caller asking for freshness read a stale result as fresh: say it, and mark
    // the response incomplete with a named degradation.
    let refresh_skipped = fresh && pipeline.is_read_only();
    if refresh_skipped {
        eprintln!(
            "note: index opened read-only (a writer holds the lock): query-time refresh is off; \
             results come from the published generation"
        );
    }
    let result = if fresh && !refresh_skipped {
        let now = search_core::time::now_unix_s();
        pipeline.search_with_refresh(query, limit, now).await
    } else {
        pipeline.search(query, limit).await
    };
    match result {
        Ok(mut resp) => {
            if refresh_skipped {
                resp.degradations.push("query_refresh_read_only".to_string());
                resp.complete = false;
            }
            println!("{}", serde_json::to_string_pretty(&resp).unwrap());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("search failed: {e}");
            ExitCode::from(1)
        }
    }
}

/// One Stage-3 background refresh tick: build the due sources the pipeline knows
/// and dispatch the scheduler's bounded refresh work, then print the report.
async fn run_refresh() -> ExitCode {
    let cfg = config();
    let pipeline = match Pipeline::open_with_scheduler(
        &cfg,
        false,
        search_agent0::scheduler_config_from_env(),
        search_agent0::DEFAULT_REFRESH_BUDGET,
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to open pipeline: {e}");
            return ExitCode::from(1);
        }
    };
    let now = search_core::time::now_unix_s();
    let sources = match pipeline.due_sources(now) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to build refresh sources: {e}");
            return ExitCode::from(1);
        }
    };
    match pipeline.run_refresh_tick(&sources, now).await {
        Ok(report) => {
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("refresh failed: {e}");
            ExitCode::from(1)
        }
    }
}

async fn run_serve() -> ExitCode {
    let cfg = config();
    let pipeline = match search_agent0::open_honouring_read_only(&cfg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to open pipeline: {e}");
            return ExitCode::from(1);
        }
    };
    let pipeline = std::sync::Arc::new(pipeline);
    // One root, or several: `SEARCH_FEDERATE_DIRS` makes the four shards answer as one corpus.
    // With the variable unset this is exactly `pipeline.searcher()`.
    let searcher = match search_agent0::fedserve::searcher(pipeline.searcher(), &cfg.data_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to build the federated searcher: {e}");
            return ExitCode::from(1);
        }
    };
    // Deep search / document read are served from the same pipeline instance.
    let deep: std::sync::Arc<dyn search_api::DeepSearcher + Send + Sync> =
        std::sync::Arc::new(search_agent0::PipelineDeepSearcher::new(pipeline.clone()));

    // §22.1 answer generation is opt-in and env-driven. `None` (no key, or the feature
    // disabled) keeps the engine lexical-only; the search path is then unchanged.
    let answerer: Option<std::sync::Arc<dyn search_engine::Answerer + Send + Sync>> =
        search_answer::from_env()
            .map(|a| std::sync::Arc::new(a) as std::sync::Arc<dyn search_engine::Answerer + Send + Sync>);
    if answerer.is_none() {
        tracing::info!("{}", search_answer::disabled_reason().message());
    }

    // §6/§8 LLM query planning is opt-in and env-driven. `None` (no key, or the feature
    // disabled) searches only the original query; the search path is then unchanged.
    let planner: Option<std::sync::Arc<dyn search_engine::QueryPlanner + Send + Sync>> =
        match search_planner::from_env() {
            Some(p) => {
                // The model call is taken OFF the request path: a cache hit is an in-process map
                // lookup, a miss returns immediately (the original query is searched, exactly as
                // with no planner) while the plan is computed in parallel, and a repeat is served
                // with the model's probe. A synchronous model call here measured 83 s cold.
                let wait = search_agent0::planner_cache::SpeculativePlanner::wait_from_env();
                let model = p.model().to_string();
                match search_agent0::planner_cache::SpeculativePlanner::new(
                    std::sync::Arc::new(p),
                    wait,
                ) {
                    Some(speculative) => {
                        tracing::info!(
                            model = %model,
                            wait_us = wait.as_micros() as u64,
                            "query planning enabled off the request path (in-process cache; miss returns immediately)"
                        );
                        Some(
                            std::sync::Arc::new(speculative)
                                as std::sync::Arc<dyn search_engine::QueryPlanner + Send + Sync>,
                        )
                    }
                    None => {
                        tracing::warn!("could not start the planner runtime; searching verbatim");
                        None
                    }
                }
            }
            None => {
                tracing::info!("{}", search_planner::disabled_reason().message());
                None
            }
        };

    let mut extra_lanes = build_dense_lane(&cfg);
    extra_lanes.extend(build_graph_lane(&cfg));
    extra_lanes.extend(build_concept_lane(&cfg));
    if let Err(e) = search_api::serve_with(&cfg, searcher, Some(deep), answerer, planner, extra_lanes).await {
        eprintln!("server error: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// `agent0 export <dir> [--limit N] [--raw] [--name NAME] [--version V]`
///
/// Compile the live index into a portable dataset pack. By default each page's
/// text is structured by the cheap LLM (`search-organize`: on-disk response
/// cache, single-flight, bounded parallelism); `--raw` skips the model and
/// writes the crawler's own fields.
async fn run_export(args: &[String]) -> ExitCode {
    let out_dir = match args.get(2) {
        Some(d) => std::path::PathBuf::from(d),
        None => return usage("agent0 export <out_dir> [--limit N] [--raw] [--name NAME]"),
    };
    let limit = flag_usize(args, "--limit").unwrap_or(usize::MAX);
    let raw = args.iter().any(|a| a == "--raw");
    let name = flag_str(args, "--name").unwrap_or_else(|| "web".to_string());
    let version = flag_str(args, "--version").unwrap_or_else(|| "1".to_string());

    let cfg = config();
    // Read-only: an export reads stored documents and writes only the pack, so it must not take the
    // writer lock from a running server or crawl (the same reason `graph build` opens read-only).
    let pipeline = match Pipeline::open_read_only(&cfg, false) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to open pipeline: {e}");
            return ExitCode::from(1);
        }
    };
    let docs = match pipeline.all_docs() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("failed to read the index: {e}");
            return ExitCode::from(1);
        }
    };

    // The index is the source of truth: its STOREd fields carry url/title/body.
    let (pages, skipped) = search_agent0::portable::pages_from_docs(&docs, limit);

    // Incremental, resumable export. Each chunk of pages is organised, written to disk and
    // checkpointed (a shard + the manifest), so a kill — `timeout`, a reboot, a crash — keeps
    // everything written so far and the next run *continues* instead of starting from zero.
    // Before this, the whole record set was buffered and written once at the end: a 6-hour export
    // of ~87k pages produced an empty directory every time and could never finish.
    const CHUNK: usize = 500;
    let mut writer = match search_dataset::PackWriter::open(&out_dir, &name, &version, CHUNK) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("failed to open the pack at {}: {e}", out_dir.display());
            return ExitCode::from(1);
        }
    };
    let already = match writer.existing_ids() {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("failed to read the existing pack: {e}");
            return ExitCode::from(1);
        }
    };
    let resumed = !already.is_empty();
    // Resume by content: a page whose record id is already on disk is not exported twice.
    let pending: Vec<search_organize::RawPage> = pages
        .into_iter()
        .filter(|p| !already.contains(&search_organize::id_from_url(&p.url)))
        .collect();

    let mode = if raw { "raw" } else { "organized" };
    if resumed {
        println!(
            "resuming: {} record(s) already in {}, {} page(s) left to do",
            writer.records(),
            out_dir.display(),
            pending.len()
        );
    }

    // The organize client is built once, and only when the model is actually needed.
    let client = if raw {
        None
    } else {
        let llm_cfg = match search_organize::LlmConfig::from_env() {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "{e} (set SEARCH_LLM_API_KEY or CROWBOT_API_KEY, or pass --raw to skip the model)"
                );
                return ExitCode::from(1);
            }
        };
        let concurrency = std::env::var("SEARCH_LLM_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(8);
        let cache_dir = std::env::var("SEARCH_LLM_CACHE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| cfg.data_dir.join("llm-cache"));
        match search_organize::LlmClient::new(llm_cfg, cache_dir, concurrency) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("failed to build the organize client: {e}");
                return ExitCode::from(1);
            }
        }
    };

    let total_chunks = pending.len().div_ceil(CHUNK);
    let mut failed_pages = 0usize;
    if !pending.is_empty() {
        println!(
            "organizing {} page(s) with the cheap model ({} chunk(s) of {CHUNK}); \
             cached completions are free re-runs",
            pending.len(),
            total_chunks
        );
    }

    for (i, chunk) in pending.chunks(CHUNK).enumerate() {
        let records: Vec<search_dataset::Record> = match client.as_ref() {
            // `--raw`: the crawler's own fields, no model.
            None => chunk.iter().map(search_agent0::portable::raw_record).collect(),
            Some(client) => {
                let (records, failures) =
                    search_organize::organize_all_lenient(client, chunk.to_vec()).await;
                for f in &failures {
                    eprintln!("organize: page failed ({}): {}", f.url, f.error);
                }
                failed_pages += failures.len();
                records
            }
        };

        for record in records {
            if let Err(e) = writer.push(record) {
                eprintln!("failed to write a record: {e}");
                return ExitCode::from(1);
            }
        }
        // Checkpoint: the shard and its manifest entry are on disk before the next chunk starts.
        match writer.flush() {
            Ok(m) => println!(
                "checkpoint {}/{}: {} page(s) exported, {} record(s) on disk in {}",
                i + 1,
                total_chunks,
                ((i + 1) * CHUNK).min(pending.len()),
                m.records,
                out_dir.display()
            ),
            Err(e) => {
                eprintln!("failed to flush the pack: {e}");
                return ExitCode::from(1);
            }
        }
    }

    if let Some(client) = client.as_ref() {
        let stats = client.cache_stats();
        eprintln!(
            "cache: {} hit(s), {} miss(es), {} entr(ies)",
            stats.hits, stats.misses, stats.entries
        );
    }

    if writer.records() == 0 {
        eprintln!("no record could be written; refusing to report an empty pack");
        return ExitCode::from(1);
    }
    let manifest = match writer.finish() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("failed to finalise the pack at {}: {e}", out_dir.display());
            return ExitCode::from(1);
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "pack": out_dir.display().to_string(),
            "name": name,
            "version": version,
            "mode": mode,
            "records": manifest.records,
            "pages_skipped": skipped,
            "pages_failed": failed_pages,
            "resumed": resumed,
            "manifest": manifest,
        }))
        .unwrap()
    );
    ExitCode::SUCCESS
}

/// `agent0 import <dir> [--limit N]`
///
/// Load a portable dataset pack into this machine's index — no network, no
/// robots, no frontier. Idempotent: re-importing a pack reports `unchanged`.
async fn run_import(args: &[String]) -> ExitCode {
    let dir = match args.get(2) {
        Some(d) => std::path::PathBuf::from(d),
        None => return usage("agent0 import <pack_dir> [--limit N]"),
    };
    let limit = flag_usize(args, "--limit").unwrap_or(usize::MAX);

    let cfg = config();
    let pipeline = match Pipeline::open(&cfg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to open pipeline: {e}");
            return ExitCode::from(1);
        }
    };
    let records = match search_dataset::read_pack(&dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("failed to read the pack at {}: {e}", dir.display());
            return ExitCode::from(1);
        }
    };

    // Bulk ingest: buffer records and commit them to `base` once per batch (spec §6's batch path)
    // instead of committing and publishing per record. The previous per-record path called
    // `publish_manifest` once per document, and that publish scans the whole index for its doc
    // count, so import cost grew quadratically with corpus size — measured at 12 docs/s on a fresh
    // index falling to 5.6 docs/s by 7.4k docs.
    let batch = std::env::var("SEARCH_BULK_BATCH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(512);
    pipeline.enable_bulk_ingest(batch);

    let mut imported = 0usize;
    let mut unchanged = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    for rec in records.into_iter().take(limit) {
        let url = match rec.url.as_deref() {
            Some(u) if !u.trim().is_empty() => u.to_string(),
            _ => {
                skipped += 1;
                continue;
            }
        };
        if rec.text.trim().is_empty() {
            skipped += 1;
            continue;
        }
        let ir = search_agent0::pipeline::ImportRecord {
            url,
            title: rec.title.clone(),
            text: rec.text.clone(),
            source: rec.source.clone(),
            published_at: rec.published_at,
            extras: rec.extras.clone(),
        };
        match pipeline.import_record(&ir) {
            Ok(search_agent0::IngestOutcome::Ingested { .. }) => imported += 1,
            Ok(search_agent0::IngestOutcome::Unchanged { .. }) => unchanged += 1,
            Ok(_) => skipped += 1,
            Err(e) => {
                failed += 1;
                eprintln!("import failed for {}: {e}", ir.url);
            }
        }
    }
    // Flush the tail so the last partial batch is committed, published and searchable. Without
    // this the final <batch-size records stay invisible — the crawl path has the same requirement.
    if let Err(e) = pipeline.flush_overlay_batch() {
        eprintln!("failed to flush the final batch: {e}");
        return ExitCode::from(1);
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "pack": dir.display().to_string(),
            "imported": imported,
            "unchanged": unchanged,
            "skipped": skipped,
            "failed": failed,
        }))
        .unwrap()
    );
    if failed > 0 {
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn outcome_json(o: &search_agent0::IngestOutcome) -> serde_json::Value {
    use search_agent0::IngestOutcome::*;
    match o {
        Ingested {
            url,
            resource_id,
            docver_id,
            version_no,
            content_id,
            generation,
        } => serde_json::json!({
            "status": "ingested", "url": url, "resource_id": resource_id,
            "docver_id": docver_id, "version_no": version_no,
            "content_id": content_id, "generation": generation,
        }),
        Unchanged {
            url,
            resource_id,
            docver_id,
            version_no,
            content_id,
            generation,
        } => serde_json::json!({
            "status": "unchanged", "url": url, "resource_id": resource_id,
            "docver_id": docver_id, "version_no": version_no,
            "content_id": content_id, "generation": generation,
        }),
        Denied {
            url,
            resource_id,
            reason,
        } => serde_json::json!({
            "status": "denied", "url": url, "resource_id": resource_id, "reason": reason,
        }),
        Routed {
            url,
            resource_id,
            route,
            discovered,
        } => serde_json::json!({
            "status": "routed", "url": url, "resource_id": resource_id,
            "route": route, "discovered": discovered,
        }),
    }
}
