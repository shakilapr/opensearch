//! `agent0 features train|build` — the v2 sparse feature layer, end to end.
//!
//! ```text
//! agent0 features train [--vectors PATH[,PATH...]] [--out DIR] [--features N] [--active N]
//!                       [--epochs N] [--sample N] [--lr F] [--log-every N]
//! agent0 features build [--dir PATH]... [--encoder PATH] [--active N] [--limit N]
//!                       [--forward-index]
//! ```
//!
//! `train` learns Model B from the teacher embeddings the fleet already carries
//! (`vectors.bin`, written by `agent0 embed build`). `build` translates the existing index model
//! into the v2 one: it reads each shard's document vectors, projects them through `W`, and writes
//! the inverted feature postings plus — with `--forward-index` — the `doc_feature` rows v2 §9
//! keeps "to support efficient updates and reranking".
//!
//! Both verbs are read-only with respect to the Tantivy index, so they run beside a crawl and
//! beside the server, exactly as the vector backfill does.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rayon::prelude::*;

use search_core::config::Config;
use search_embed::VectorStore;
use search_features::{train, Encoder, SparseIndex, TrainOptions};

/// Parse `--name value` pairs into a map; unknown flags are reported by the caller.
fn flags(args: &[String]) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut i = 2; // args[0] = program, args[1] = "features", args[2] = verb
    while i < args.len() {
        let a = &args[i];
        if let Some(name) = a.strip_prefix("--") {
            let (name, inline) = match name.split_once('=') {
                Some((n, v)) => (n.to_string(), Some(v.to_string())),
                None => (name.to_string(), None),
            };
            match inline {
                Some(v) => {
                    out.insert(name, v);
                    i += 1;
                }
                None => {
                    let v = args.get(i + 1).cloned().unwrap_or_default();
                    out.insert(name, v);
                    i += 2;
                }
            }
        } else {
            i += 1;
        }
    }
    out
}

fn usize_flag(f: &std::collections::BTreeMap<String, String>, k: &str, d: usize) -> usize {
    f.get(k).and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn f32_flag(f: &std::collections::BTreeMap<String, String>, k: &str, d: f32) -> f32 {
    f.get(k).and_then(|v| v.parse().ok()).unwrap_or(d)
}

/// Every root that is served: the primary data dir plus `SEARCH_FEDERATE_DIRS`.
fn roots(cfg: &Config) -> Vec<PathBuf> {
    let mut dirs = vec![cfg.data_dir.clone()];
    dirs.extend(crate::fedserve::dirs_from_env(&cfg.data_dir));
    dirs.dedup();
    dirs
}

fn default_encoder(cfg: &Config) -> PathBuf {
    cfg.data_dir.join("features").join("encoder.bin")
}

/// `agent0 features train`
pub fn run_train(args: &[String]) -> ExitCode {
    let cfg = Config::from_env();
    let f = flags(args);
    let out = f
        .get("out")
        .map(PathBuf::from)
        .unwrap_or_else(|| cfg.data_dir.join("features"));
    let encoder_path = out.join("encoder.bin");

    let vector_paths: Vec<PathBuf> = match f.get("vectors") {
        Some(list) => list.split(',').map(PathBuf::from).collect(),
        None => {
            let mut v: Vec<PathBuf> = roots(&cfg)
                .into_iter()
                .map(|d| d.join("vectors.bin"))
                .collect();
            v.retain(|p| p.is_file());
            v
        }
    };
    if vector_paths.is_empty() {
        eprintln!(
            "no `vectors.bin` found. Build them first: agent0 embed build --out <root>/vectors.bin"
        );
        return ExitCode::from(2);
    }

    let sample_cap = usize_flag(&f, "sample", 400_000);
    let mut samples: Vec<Vec<f32>> = Vec::new();
    let mut dim = 0usize;
    for path in &vector_paths {
        match VectorStore::load(path) {
            Ok(store) => {
                if dim == 0 {
                    dim = store.dim();
                }
                if store.dim() != dim {
                    eprintln!(
                        "{}: {}-dimensional, expected {dim}; skipping (vectors from two models \
                         cannot be mixed)",
                        path.display(),
                        store.dim()
                    );
                    continue;
                }
                let per_file = (sample_cap / vector_paths.len()).max(1);
                let stride = (store.len() / per_file).max(1);
                let before = samples.len();
                for (i, e) in store.entries().iter().enumerate() {
                    if i % stride == 0 && samples.len() < sample_cap {
                        samples.push(e.v.clone());
                    }
                }
                eprintln!(
                    "{}: {} documents, sampled {}",
                    path.display(),
                    store.len(),
                    samples.len() - before
                );
            }
            Err(e) => {
                eprintln!("{}: {e}", path.display());
                return ExitCode::from(1);
            }
        }
    }
    if samples.is_empty() {
        eprintln!("no samples read");
        return ExitCode::from(1);
    }

    let opts = TrainOptions {
        features: usize_flag(&f, "features", 4096),
        active: usize_flag(&f, "active", 16),
        epochs: usize_flag(&f, "epochs", 6),
        batch: usize_flag(&f, "batch", 2048),
        lr: f32_flag(&f, "lr", 0.01),
        momentum: f32_flag(&f, "momentum", 0.9),
        seed: usize_flag(&f, "seed", 0x5eed_0001) as u64,
        log_every: usize_flag(&f, "log-every", 0) as u64,
        resample_every: usize_flag(&f, "resample-every", 20_000),
        // Off by default; see `TrainOptions::train_bias`. `--train-bias` opts into the
        // winner-take-all dynamic the default exists to avoid.
        train_bias: f.contains_key("train-bias"),
    };
    let started = std::time::Instant::now();
    eprintln!(
        "training: {} samples × {dim} dims → {} features, {} active, {} epochs, lr {}",
        samples.len(),
        opts.features,
        opts.active,
        opts.epochs,
        opts.lr
    );
    let encoder = match train(&samples, dim, &opts) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("training failed: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = encoder.save(&encoder_path) {
        eprintln!("writing {}: {e}", encoder_path.display());
        return ExitCode::from(1);
    }
    let used = occupancy(&encoder, &samples);
    println!(
        "trained {} on {} samples in {:.1}s\n  output        : {}\n  shape         : {} features × {} dims\n  active        : {}\n  features used : {} of {} ({:.1}%)",
        encoder_path.display(),
        samples.len(),
        started.elapsed().as_secs_f32(),
        encoder_path.display(),
        encoder.features(),
        encoder.dim(),
        encoder.active(),
        used,
        encoder.features(),
        100.0 * used as f32 / encoder.features() as f32
    );
    ExitCode::SUCCESS
}

/// How many features actually fire across the sample set.
///
/// A vocabulary where a handful of ids absorb every code is the failure mode that makes a sparse
/// index useless (all documents land in the same posting lists). Reporting it at train time is
/// cheaper than discovering it in retrieval.
fn occupancy(encoder: &Encoder, samples: &[Vec<f32>]) -> usize {
    let mut seen = vec![false; encoder.features()];
    for s in samples.iter().take(20_000) {
        if let Ok(hits) = encoder.encode(s, encoder.active()) {
            for h in hits {
                seen[h.feature as usize] = true;
            }
        }
    }
    seen.iter().filter(|x| **x).count()
}

/// `agent0 features build`
pub fn run_build(args: &[String]) -> ExitCode {
    let cfg = Config::from_env();
    let f = flags(args);
    let dirs: Vec<PathBuf> = match f.get("dir") {
        Some(d) => vec![PathBuf::from(d)],
        None => roots(&cfg),
    };
    // `--dir` may repeat; `flags` keeps the last, so read every occurrence explicitly.
    let mut dirs = dirs;
    for pair in args.windows(2) {
        if pair[0] == "--dir" && !pair[1].starts_with("--") {
            let p = PathBuf::from(&pair[1]);
            if !dirs.contains(&p) {
                dirs.push(p);
            }
        }
    }

    let encoder_path = f
        .get("encoder")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_encoder(&cfg));
    let encoder = match Encoder::load(&encoder_path) {
        Ok(e) => e,
        Err(e) => {
            eprintln!(
                "{}: {e} (train one with `agent0 features train`)",
                encoder_path.display()
            );
            return ExitCode::from(2);
        }
    };
    let active = usize_flag(&f, "active", encoder.active());
    let limit = f.get("limit").and_then(|v| v.parse::<usize>().ok());
    let forward = f.contains_key("forward-index");
    let model_id = f
        .get("model-id")
        .cloned()
        .unwrap_or_else(|| "model-b".to_string());
    let version = usize_flag(&f, "version", 1) as i64;

    let mut built = 0usize;
    for dir in &dirs {
        let vectors = dir.join("vectors.bin");
        if !vectors.is_file() {
            eprintln!(
                "{}: no vectors.bin (run `agent0 embed build --out {}` first)",
                dir.display(),
                vectors.display()
            );
            continue;
        }
        let store = match VectorStore::load(&vectors) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{}: {e}", vectors.display());
                return ExitCode::from(1);
            }
        };
        if store.dim() != encoder.dim() {
            eprintln!(
                "{}: vectors are {}-dimensional but the encoder takes {}; skipping",
                dir.display(),
                store.dim(),
                encoder.dim()
            );
            continue;
        }
        let started = std::time::Instant::now();
        // Sort by document id *first*, then encode chunk by chunk. `SparseIndex::build` would
        // collect every vector — for a 350K-document shard that is the store plus a second copy of
        // it, and the process was killed by the memory cgroup (4.1 GB RSS) before it finished.
        // Only ids are held here: ~30 bytes per document, versus 1.5 KB per vector.
        let mut order: Vec<usize> = (0..store.len()).collect();
        order.sort_by(|a, b| store.entries()[*a].doc_id.cmp(&store.entries()[*b].doc_id));
        order.dedup_by(|a, b| {
            let same = store.entries()[*a].doc_id == store.entries()[*b].doc_id;
            if same {
                // Keep the first occurrence; `dedup_by` removes `a` when it returns true.
                *b = *a;
            }
            same
        });
        if let Some(n) = limit {
            order.truncate(n);
        }

        let mut index = SparseIndex::new(encoder.features());
        let mut forward_writer = match (forward, ForwardWriter::new(dir, &model_id, version, encoder.features())) {
            (true, Ok(w)) => Some(w),
            (true, Err(e)) => {
                eprintln!("  forward index failed: {e}");
                return ExitCode::from(1);
            }
            (false, _) => None,
        };
        let mut chunk_rows = 0usize;
        for chunk in order.chunks(BUILD_CHUNK) {
            // Encode the chunk in parallel, then push in id order: the postings' doc indices must
            // ascend, and the output file must stay byte-identical between runs.
            let coded: Vec<Vec<search_features::FeatureHit>> = match chunk
                .par_iter()
                .map(|&i| encoder.encode(&store.entries()[i].v, active))
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{}: encode failed: {e}", dir.display());
                    return ExitCode::from(1);
                }
            };
            let mut batch: Vec<(String, Vec<search_features::FeatureHit>)> = Vec::with_capacity(chunk.len());
            for (&i, hits) in chunk.iter().zip(coded.iter()) {
                let e = &store.entries()[i];
                index.push_coded(e.doc_id.clone(), e.source_id.clone(), hits);
                batch.push((e.doc_id.clone(), hits.to_vec()));
            }
            if let Some(w) = forward_writer.as_mut() {
                match w.put_batch(&batch) {
                    Ok(n) => chunk_rows += n,
                    Err(err) => {
                        eprintln!("  forward index failed: {err}");
                        return ExitCode::from(1);
                    }
                }
            }
        }
        let index = index;
        let forward_rows = match forward_writer {
            Some(w) => match w.finish() {
                Ok(()) => chunk_rows,
                Err(e) => {
                    eprintln!("  forward index failed: {e}");
                    return ExitCode::from(1);
                }
            },
            None => 0,
        };
        let out = dir.join("features").join("postings.bin");
        if let Err(e) = index.save(&out) {
            eprintln!("{}: {e}", out.display());
            return ExitCode::from(1);
        }
        let per_doc = if index.len() > 0 {
            index.posting_count() as f64 / index.len() as f64
        } else {
            0.0
        };
        println!(
            "{}: {} documents, {} postings ({per_doc:.1}/doc), {} features, {:.1}s → {}",
            dir.display(),
            index.len(),
            index.posting_count(),
            index.features(),
            started.elapsed().as_secs_f32(),
            out.display()
        );
        if forward {
            println!(
                "  forward index: {} feature rows, {forward_rows} doc_feature rows",
                encoder.features()
            );
        }
        built += 1;
    }
    if built == 0 {
        eprintln!("nothing built");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// How many documents a streaming build encodes and writes at a time.
///
/// 8,192 documents is ~13 MB of vectors plus ~1 MB of codes: small enough that the build's memory
/// is dominated by the vector store it is reading rather than by a second copy of it, large enough
/// that the per-chunk transaction is amortised over ~131K rows.
const BUILD_CHUNK: usize = 8_192;

/// Writes `feature` and `doc_feature` rows while a build is streaming (v2 §4/§5.2/§9).
///
/// `doc_feature` has a foreign key to `resource`, and a document can legitimately be absent from
/// a shard's metadata (a vector sidecar built from a pre-rebuild index). Those rows are skipped by
/// the `WHERE EXISTS` in the insert and simply do not count, rather than aborting the whole shard.
///
/// One transaction per chunk rather than per document: a per-row implicit transaction on 5.6M
/// rows is minutes of fsync, and one transaction for the whole shard is the memory the streaming
/// build exists to avoid.
struct ForwardWriter {
    conn: rusqlite::Connection,
    model_id: String,
    version: i64,
    space_id: String,
    now: i64,
}

impl ForwardWriter {
    fn new(dir: &Path, model_id: &str, version: i64, features: usize) -> Result<Self, String> {
        let db = dir.join("meta").join("meta.sqlite");
        if !db.is_file() {
            return Err(format!("{} does not exist", db.display()));
        }
        let conn = rusqlite::Connection::open(&db).map_err(|e| e.to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(30)).map_err(|e| e.to_string())?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let space_id = format!("{model_id}-v{version}");
        let w = Self { conn, model_id: model_id.to_string(), version, space_id, now };
        w.declare_features(features)?;
        Ok(w)
    }

    /// Register the `feature` rows. Idempotent, so a rebuild of the same space is safe.
    fn declare_features(&self, features: usize) -> Result<(), String> {
        let tx = self.conn.unchecked_transaction().map_err(|e| e.to_string())?;
        {
            let mut feat = tx
                .prepare(
                    "INSERT OR REPLACE INTO feature
                       (feature_id, model_id, version, space_id, label, created_at)
                     VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
                )
                .map_err(|e| e.to_string())?;
            for f in 0..features as i64 {
                feat.execute(rusqlite::params![f, self.model_id, self.version, self.space_id, self.now])
                    .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())
    }

    /// Write one chunk's rows; returns how many rows actually landed.
    fn put_batch(&mut self, batch: &[(String, Vec<search_features::FeatureHit>)]) -> Result<usize, String> {
        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        let mut rows = 0usize;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO doc_feature
                       (resource_id, feature_id, model_id, version, space_id, activation, created_at)
                     SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7
                      WHERE EXISTS (SELECT 1 FROM resource r WHERE r.resource_id = ?1)",
                )
                .map_err(|e| e.to_string())?;
            for (doc_id, hits) in batch {
                for h in hits {
                    rows += stmt
                        .execute(rusqlite::params![
                            doc_id,
                            h.feature as i64,
                            self.model_id,
                            self.version,
                            self.space_id,
                            h.activation as f64,
                            self.now
                        ])
                        .map_err(|e| e.to_string())?;
                }
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(rows)
    }

    /// No trailing work: every chunk committed as it was written, so an interrupted build leaves
    /// the rows it finished rather than nothing.
    fn finish(self) -> Result<(), String> {
        Ok(())
    }
}
