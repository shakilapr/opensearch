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
/// `agent0 features probe` — how often does the lane alone reach the labelled document?
///
/// A fusion weight can only help if the lane's candidate list contains the answer. Measuring that
/// directly separates two very different failures: a lane whose target is *absent* from its top-k
/// cannot be fixed by tuning weights (the features do not encode the relationship), while a lane
/// that merely ranks it too low can be. Reported as recall@k for several k plus the median rank of
/// the target when it is found.
pub fn run_probe(args: &[String]) -> ExitCode {
    let f = flags(args);
    let set = f.get("set").cloned().unwrap_or("benchmarks/queries.corpus.jsonl".to_string());
    let cap = usize_flag(&f, "k", 500);
    let encoder_path = std::path::PathBuf::from(
        f.get("encoder").cloned().unwrap_or("/var/lib/search/features/encoder.bin".to_string()),
    );
    // `--dirs a:b:c` rather than a repeated `--dir`: the flag parser here is a map, and a repeated
    // key would silently keep only the last value.
    let mut dirs: Vec<std::path::PathBuf> = f
        .get("dirs")
        .map(|raw| raw.split(':').map(std::path::PathBuf::from).collect())
        .unwrap_or_default();
    if dirs.is_empty() {
        dirs.push(std::path::PathBuf::from("/var/lib/search"));
    }

    #[cfg(feature = "bge")]
    let embedder: std::sync::Arc<dyn search_embed::Embedder> = {
        let cache = std::env::var("SEARCH_MODEL_CACHE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("/var/lib/search/fastembed"));
        match search_embed::bge::BgeEmbedder::with_cache_dir(cache) {
            Ok(e) => std::sync::Arc::new(e),
            Err(e) => {
                eprintln!("probe needs the BGE weights the encoder was trained against: {e}");
                return ExitCode::from(1);
            }
        }
    };
    #[cfg(not(feature = "bge"))]
    {
        eprintln!("probe requires building with --features bge: the lane's query side must be the model the postings were built from");
        return ExitCode::from(1);
    }

    let lane = match search_features::FeatureLane::from_paths(&encoder_path, &dirs, embedder) {
        Ok(Some(l)) => l,
        Ok(None) => {
            eprintln!("no postings under {:?}", dirs);
            return ExitCode::from(1);
        }
        Err(e) => {
            eprintln!("lane failed to load: {e}");
            return ExitCode::from(1);
        }
    };
    let cases = match search_eval::load_cases(std::path::Path::new(&set)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to load {set}: {e}");
            return ExitCode::from(1);
        }
    };
    println!(
        "probe: {} cases, lane over {} documents across {} shard(s), k={cap}",
        cases.len(),
        lane.len(),
        lane.shard_summary().len()
    );

    let ks = [10usize, 50, 100, cap];
    let mut hits = [0usize; 4];
    let mut ranks: Vec<f64> = Vec::new();
    let mut empty = 0usize;
    for case in &cases {
        let found = match search_retrieval::LaneSearcher::search(&lane, &case.query, cap, None) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("lane error: {e}");
                return ExitCode::from(1);
            }
        };
        if found.is_empty() {
            empty += 1;
        }
        let rank = found
            .iter()
            .position(|c| case.relevant.iter().any(|r| r == &c.doc_id))
            .map(|p| p + 1);
        if let Some(r) = rank {
            ranks.push(r as f64);
            for (i, k) in ks.iter().enumerate() {
                if r <= *k {
                    hits[i] += 1;
                }
            }
        }
    }
    let n = cases.len().max(1) as f64;
    for (i, k) in ks.iter().enumerate() {
        println!("  recall@{k:<4}: {:.4}", hits[i] as f64 / n);
    }
    ranks.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = ranks.get(ranks.len() / 2).copied().unwrap_or(f64::NAN);
    println!(
        "  found      : {} of {} (median rank {median:.0})",
        ranks.len(),
        cases.len()
    );
    println!("  empty lane : {empty} queries returned no candidates");
    ExitCode::SUCCESS
}

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
    // How a document's activations are scaled before quantisation. `max` makes every document's
    // strongest feature exactly 1.0, which throws away how *well* a document is represented;
    // `l2` keeps the direction and a magnitude that is comparable across documents (a cosine-ish
    // score), and `none` keeps the raw ReLU output clipped at 1.0. The query side stays
    // max-normalised in every case: a query is a direction, while a document has a degree.
    let norm = std::env::var("SEARCH_FEATURE_NORM").unwrap_or_else(|_| "max".to_string());
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
        // A document can own several vectors (`res_x:1`, `res_x:2`, … for its text chunks), and the
        // pipeline's document ids are the bare resource. Postings keyed on the chunk id could never
        // be joined by anything downstream: measured on the shards, every posting carried a `:1`
        // suffix while `POST /search` returns bare ids, so the fleet lane's hits were unresolvable
        // by construction. Chunks of one document are adjacent in the sorted order, so the code is
        // merged per document and pushed once — that also keeps a document from appearing twice in
        // its own postings list.
        let mut pending: Option<(String, Option<String>, std::collections::BTreeMap<u32, f32>)> =
            None;
        let mut batch: Vec<(String, Vec<search_features::FeatureHit>)> = Vec::new();
        for chunk in order.chunks(BUILD_CHUNK) {
            // Encode the chunk in parallel, then push in id order: the postings' doc indices must
            // ascend, and the output file must stay byte-identical between runs.
            let coded: Vec<Vec<search_features::FeatureHit>> = match chunk
                .par_iter()
                .map(|&i| encoder.code_topk(&store.entries()[i].v, active))
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{}: encode failed: {e}", dir.display());
                    return ExitCode::from(1);
                }
            };
            for (&i, raw) in chunk.iter().zip(coded.into_iter()) {
                let e = &store.entries()[i];
                let base = base_resource_id(&e.doc_id).to_string();
                let same = matches!(&pending, Some((id, _, _)) if *id == base);
                if !same {
                    if let Some((id, source, merged)) = pending.take() {
                        emit_doc(&mut index, &mut batch, id, source, merged, active, &norm);
                    }
                    pending = Some((base, e.source_id.clone(), std::collections::BTreeMap::new()));
                }
                if let Some((_, _, merged)) = pending.as_mut() {
                    for h in raw {
                        let slot = merged.entry(h.feature).or_insert(f32::MIN);
                        if h.activation > *slot {
                            *slot = h.activation;
                        }
                    }
                }
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
            batch.clear();
        }
        if let Some((id, source, merged)) = pending.take() {
            emit_doc(&mut index, &mut batch, id, source, merged, active, &norm);
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

/// Turn one document's merged activations into its code and push it into both indexes.
///
/// The `active` strongest features survive, which is the document's own TopK — merging chunks first
/// and truncating after is what makes a long document's code represent the document rather than one
/// arbitrary chunk of it.
fn emit_doc(
    index: &mut search_features::SparseIndex,
    batch: &mut Vec<(String, Vec<search_features::FeatureHit>)>,
    doc_id: String,
    source_id: Option<String>,
    merged: std::collections::BTreeMap<u32, f32>,
    active: usize,
    norm: &str,
) {
    let mut hits: Vec<search_features::FeatureHit> = merged
        .into_iter()
        .map(|(feature, activation)| search_features::FeatureHit { feature, activation })
        .collect();
    hits.sort_by(|a, b| {
        b.activation
            .partial_cmp(&a.activation)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.feature.cmp(&b.feature))
    });
    hits.truncate(active);
    let hits = rescale(hits, norm);
    index.push_coded(doc_id.clone(), source_id, &hits);
    batch.push((doc_id, hits));
}

/// Scale one document's code according to the normalisation the build was asked for.
///
/// Every mode leaves the *ranking inside a document* alone and only changes the document's overall
/// weight, which is exactly the quantity [`search_features::SparseIndex::search`] compares across
/// documents when it accumulates `query · document · idf`.
fn rescale(mut hits: Vec<search_features::FeatureHit>, mode: &str) -> Vec<search_features::FeatureHit> {
    match mode {
        "l2" => {
            let sum: f32 = hits.iter().map(|h| h.activation * h.activation).sum();
            let norm = sum.sqrt();
            if norm > 0.0 {
                for h in &mut hits {
                    h.activation /= norm;
                }
            }
            hits
        }
        "none" => {
            for h in &mut hits {
                h.activation = h.activation.min(1.0);
            }
            hits
        }
        _ => {
            let max = hits.first().map(|h| h.activation).unwrap_or(0.0);
            if max > 0.0 {
                for h in &mut hits {
                    h.activation /= max;
                }
            }
            hits
        }
    }
}

/// The resource a vector belongs to: `res_<hash>:3` (chunk 3) is a chunk of `res_<hash>`.
///
/// Only a trailing `:<digits>` is stripped. An id with no suffix is already a resource id, and a
/// colon anywhere else is left alone rather than guessed at.
fn base_resource_id(doc_id: &str) -> &str {
    match doc_id.rsplit_once(':') {
        Some((base, suffix)) if !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()) => {
            base
        }
        _ => doc_id,
    }
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
    ///
    /// Document ids are normalised to the owning resource: a fleet vector is keyed
    /// `res_<hash>:<chunk>` (one vector per text chunk, which is also what Tantivy's delete term
    /// holds), while `resource.resource_id` is the bare `res_<hash>`. Writing the suffixed id made
    /// the `WHERE EXISTS` guard skip almost every row — on shard-1 only 27,815 of 411,815 documents
    /// landed, which is why the mirror is keyed on the base id and the per-chunk codes are merged.
    ///
    /// Merge rule: per feature, the highest activation across the document's chunks. A feature
    /// present in one chunk and absent from another is still a property of the *document*, and
    /// `max` keeps the strongest evidence rather than averaging it away. The upsert repeats the
    /// merge across batches, because a long document's chunks can straddle a chunk boundary.
    fn put_batch(&mut self, batch: &[(String, Vec<search_features::FeatureHit>)]) -> Result<usize, String> {
        let mut merged: std::collections::BTreeMap<&str, std::collections::BTreeMap<u32, f32>> =
            std::collections::BTreeMap::new();
        for (doc_id, hits) in batch {
            let base = base_resource_id(doc_id);
            let slot = merged.entry(base).or_default();
            for h in hits {
                let e = slot.entry(h.feature).or_insert(f32::MIN);
                if h.activation > *e {
                    *e = h.activation;
                }
            }
        }

        let tx = self.conn.transaction().map_err(|e| e.to_string())?;
        let mut rows = 0usize;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO doc_feature
                       (resource_id, feature_id, model_id, version, space_id, activation, created_at)
                     SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7
                      WHERE EXISTS (SELECT 1 FROM resource r WHERE r.resource_id = ?1)
                     ON CONFLICT (resource_id, model_id, version, space_id, feature_id)
                     DO UPDATE SET activation = max(doc_feature.activation, excluded.activation)",
                )
                .map_err(|e| e.to_string())?;
            for (doc_id, hits) in &merged {
                for (feature, activation) in hits {
                    rows += stmt
                        .execute(rusqlite::params![
                            doc_id,
                            *feature as i64,
                            self.model_id,
                            self.version,
                            self.space_id,
                            *activation as f64,
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
