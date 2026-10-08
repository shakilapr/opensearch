//! `agent0` — Stage-0 command line.
//!
//! ```text
//! agent0 ingest <url>            one URL through the full pipeline
//! agent0 search <query> [limit]  query the live index
//! agent0 serve                   start the axum /search API over the index
//! ```
//!
//! Configuration comes from the environment via `search_core::Config::from_env`
//! (data dir, bind address, limits, user agent).

use std::process::ExitCode;

use search_agent0::Pipeline;
use search_core::config::Config;

#[tokio::main]
async fn main() -> ExitCode {
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
        Some("refresh") => run_refresh().await,
        _ => usage(
            "agent0 <ingest <url> | search <query> [limit] | refresh | serve>",
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
    let pipeline = match Pipeline::open(&cfg) {
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
    if let Err(e) = search_api::serve(&cfg, searcher, Some(deep)).await {
        eprintln!("server error: {e}");
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
