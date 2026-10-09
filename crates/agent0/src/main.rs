//! `agent0` — Stage-0 command line.
//!
//! ```text
//! agent0 ingest <url>            one URL through the full pipeline
//! agent0 search <query> [limit]  query the live index
//! agent0 serve                   start the axum /search API over the index
//! agent0 refresh                 one Stage-3 background refresh tick
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
fn open_for_serve(cfg: &search_core::config::Config) -> search_core::error::AppResult<Pipeline> {
    let mode = std::env::var("SEARCH_READ_ONLY").unwrap_or_default().trim().to_ascii_lowercase();
    match mode.as_str() {
        "1" | "true" | "yes" => {
            tracing::info!("SEARCH_READ_ONLY=1: serving read-only (no index lock, refresh off)");
            Pipeline::open_read_only(cfg, false)
        }
        "0" | "false" | "no" => Pipeline::open(cfg),
        _ => match Pipeline::open(cfg) {
            Ok(p) => Ok(p),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "index is locked (a crawl is running?): serving READ-ONLY — queries work, query-time refresh is off"
                );
                Pipeline::open_read_only(cfg, false)
            }
        },
    }
}

/// Build the dense lane over the vector sidecar, when one exists.
///
/// The binary owns this choice: it knows the data root, the `SEARCH_MODEL_CACHE` location and
/// whether this build has the `bge` feature. An absent sidecar is a normal state (the corpus has
/// not been backfilled) and yields no lane; a *mismatched* one is logged and skipped rather than
/// allowed to inject noise into every query.
fn build_dense_lane(
    cfg: &search_core::config::Config,
) -> Vec<std::sync::Arc<dyn search_retrieval::LaneSearcher + Send + Sync>> {
    let path = cfg.data_dir.join("vectors.bin");
    if !path.exists() {
        tracing::info!(
            path = %path.display(),
            "no vector store: the dense lane is off (build one with `agent0 embed build`)"
        );
        return Vec::new();
    }
    let embedder: std::sync::Arc<dyn search_embed::Embedder> = {
        #[cfg(feature = "bge")]
        {
            let cache = std::env::var("SEARCH_MODEL_CACHE")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| cfg.data_dir.join("fastembed"));
            match search_embed::bge::BgeEmbedder::with_cache_dir(cache) {
                Ok(e) => std::sync::Arc::new(e),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "BGE weights unavailable: serving without the dense lane"
                    );
                    return Vec::new();
                }
            }
        }
        #[cfg(not(feature = "bge"))]
        {
            std::sync::Arc::new(search_embed::HashEmbedder::new(256))
        }
    };
    match search_retrieval::DenseLane::from_path(&path, embedder) {
        Ok(Some(lane)) => {
            tracing::info!(path = %path.display(), docs = lane.len(), "dense lane enabled");
            vec![std::sync::Arc::new(lane)]
        }
        Ok(None) => Vec::new(),
        Err(e) => {
            tracing::warn!(error = %e, "vector store unusable: serving without the dense lane");
            Vec::new()
        }
    }
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
        Some("serve") => run_serve().await,
        Some("crawl") => search_agent0::crawl::run_crawl(&args).await,
        Some("graph") => match args.get(2).map(String::as_str) {
            Some("build") => search_agent0::graphbuild::run(&args).await,
            _ => {
                eprintln!("usage: agent0 graph build [--limit N] [--min-count C] [--min-docs D] [--window W]");
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
        _ => usage(
            "agent0 <ingest <url> | search <query> [limit] | refresh | serve \
             | crawl <seedsfile> [--max-docs N] [--concurrency C] [--out DIR] [--max-urls N] \
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
    let pipeline = match Pipeline::open(&cfg) {
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
    let result = if fresh {
        let now = search_core::time::now_unix_s();
        pipeline.search_with_refresh(query, limit, now).await
    } else {
        pipeline.search(query, limit).await
    };
    match result {
        Ok(resp) => {
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
    let pipeline = match open_for_serve(&cfg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to open pipeline: {e}");
            return ExitCode::from(1);
        }
    };
    let pipeline = std::sync::Arc::new(pipeline);
    let searcher = pipeline.searcher();
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

    let mut extra_lanes = build_dense_lane(&cfg);
    extra_lanes.extend(build_graph_lane(&cfg));
    if let Err(e) = search_api::serve_with(&cfg, searcher, Some(deep), answerer, extra_lanes).await {
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
    let pipeline = match Pipeline::open(&cfg) {
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

    let mut failed_pages = 0usize;
    let (records, mode): (Vec<search_dataset::Record>, &str) = if raw {
        let recs = pages.iter().map(search_agent0::portable::raw_record).collect();
        (recs, "raw")
    } else {
        let llm_cfg = match search_organize::LlmConfig::from_env() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{e} (set KITANI_API_KEY, or pass --raw to skip the model)");
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
        let client = match search_organize::LlmClient::new(llm_cfg, cache_dir, concurrency) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("failed to build the organize client: {e}");
                return ExitCode::from(1);
            }
        };
        println!(
            "organizing {} page(s) with the cheap model (concurrency {concurrency}); \
             cached completions are free re-runs",
            pages.len()
        );
        match search_organize::organize_all_lenient(&client, pages).await {
            (r, failures) => {
                let stats = client.cache_stats();
                eprintln!(
                    "cache: {} hit(s), {} miss(es), {} entr(ies)",
                    stats.hits, stats.misses, stats.entries
                );
                for f in &failures {
                    eprintln!("organize: page failed ({}): {}", f.url, f.error);
                }
                failed_pages = failures.len();
                if r.is_empty() {
                    eprintln!("organize: no page could be organized; refusing to write an empty pack");
                    return ExitCode::from(1);
                }
                (r, "organized")
            }
        }
    };

    let count = records.len();
    let manifest = match search_dataset::write_pack(&out_dir, &name, &version, records, 500) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("failed to write the pack to {}: {e}", out_dir.display());
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
            "records": count,
            "pages_skipped": skipped,
            "pages_failed": failed_pages,
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
