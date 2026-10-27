//! Wire the federated searcher into `agent0 serve` (plan-v2 Stage 1).
//!
//! One shard is served today; the corpus is four shards. `SEARCH_FEDERATE_DIRS` (a
//! colon-separated list of data roots) makes `serve` answer from all of them: the serving root
//! stays the primary (its dense and graph lanes keep working, since those are per-root artifacts),
//! and every listed root contributes its lexical index through [`FederatedSearcher`].
//!
//! Each extra root is opened **read-only** — no Tantivy writer, so no lock — which is what lets a
//! shard still be crawled while it is being served. A root that cannot be opened is reported and
//! skipped rather than aborting the server: serving three shards beats refusing to start.

use std::path::PathBuf;
use std::sync::Arc;

use search_api::{FederatedSearcher, Searcher};
use search_core::config::Config;
use search_core::{AppError, AppResult};

/// Data roots named by `SEARCH_FEDERATE_DIRS`, in order, skipping the primary root and repeats.
pub fn dirs_from_env(primary: &std::path::Path) -> Vec<PathBuf> {
    let Ok(raw) = std::env::var("SEARCH_FEDERATE_DIRS") else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for part in raw.split(':').map(str::trim).filter(|p| !p.is_empty()) {
        let path = PathBuf::from(part);
        if path == primary || out.contains(&path) {
            continue;
        }
        out.push(path);
    }
    out
}

/// Open one extra data root read-only and return its searcher, named after the root.
fn open_shard(dir: &std::path::Path) -> AppResult<(String, Arc<dyn Searcher + Send + Sync>)> {
    // Never create a shard root. `Config::ensure_dirs` would happily build an empty index at a
    // typo'd path, and an empty shard that answers "no results" is worse than a loud failure.
    if !dir.is_dir() {
        return Err(AppError::Config(format!(
            "shard {} does not exist",
            dir.display()
        )));
    }
    let mut cfg = Config::from_env();
    cfg.data_dir = dir.to_path_buf();
    let pipeline = crate::Pipeline::open_read_only(&cfg, false).map_err(|e| {
        AppError::Config(format!("shard {}: {e}", dir.display()))
    })?;
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| dir.display().to_string());
    Ok((name, pipeline.searcher()))
}

/// Build the searcher `serve` should use.
///
/// With no `SEARCH_FEDERATE_DIRS` this returns `primary` untouched — the single-shard path is
/// exactly what it was. With shards configured it returns a [`FederatedSearcher`] over the
/// primary plus every root that opened.
pub fn searcher(
    primary: Arc<dyn Searcher + Send + Sync>,
    primary_dir: &std::path::Path,
) -> AppResult<Arc<dyn Searcher + Send + Sync>> {
    let dirs = dirs_from_env(primary_dir);
    if dirs.is_empty() {
        return Ok(primary);
    }
    let mut shards: Vec<(String, Arc<dyn Searcher + Send + Sync>)> =
        vec![("primary".to_string(), primary)];
    for dir in &dirs {
        match open_shard(dir) {
            Ok(shard) => {
                tracing::info!(shard = %shard.0, path = %dir.display(), "federating shard");
                shards.push(shard);
            }
            Err(e) => {
                // Loud, and not fatal: the alternative is a server that will not start because
                // one shard's directory is missing.
                tracing::error!(path = %dir.display(), error = %e, "could not open shard; serving without it");
            }
        }
    }
    if shards.len() == 1 {
        tracing::warn!("SEARCH_FEDERATE_DIRS is set but no shard could be opened; serving one root");
        return Ok(shards.pop().expect("one shard").1);
    }
    tracing::info!(shards = shards.len(), "serving a federated index");
    Ok(Arc::new(FederatedSearcher::new(shards)?))
}
