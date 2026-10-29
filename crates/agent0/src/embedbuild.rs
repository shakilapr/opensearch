//! `agent0 embed build` — write the document-vector sidecar the dense lane reads.
//!
//! The index is the source of truth: every document's STOREd `title` + a bounded prefix of
//! `body` is embedded, keyed by `doc_id`. Two properties matter operationally:
//!
//! * **It opens the index read-only.** No writer, so no lock: a backfill runs *while a crawl is
//!   crawling the same data root*, which is what makes the dense lane practical on a corpus that
//!   is still growing. Anything already embedded keeps working; the file is simply rewritten.
//! * **It is reproducible.** Documents and vectors are sorted by `doc_id` before saving, so the
//!   same index produces byte-identical output, and re-running is a compare rather than a guess.
//!
//! The embedder is chosen the same way `search-bench` chooses one: `bge` (the real
//! BGE-small-en-v1.5 model, the default build) or `hash` (deterministic, dependency-free). BGE
//! weights are fetched into `SEARCH_MODEL_CACHE` on first use; if they cannot be loaded the
//! command fails with the model error rather than silently degrading to the lexical stand-in —
//! a vector store that claims to be semantic when it is not would be worse than no store.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use crate::pipeline::Pipeline;
use search_core::config::Config;
use search_embed::{Embedder, HashEmbedder, VectorStore};
use search_indexer::IndexDoc;

/// Characters of `body` fed to the embedder per document.
///
/// BGE-small truncates at 512 tokens anyway, and a documentation page's opening is what
/// describes it; the whole body would only add cost and truncation noise.
const DEFAULT_MAX_CHARS: usize = 2000;

fn flag_usize(args: &[String], flag: &str) -> Option<usize> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1)?.parse().ok()
}

fn flag_str(args: &[String], flag: &str) -> Option<String> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1).cloned()
}

/// The text embedded for one document: its title, then a bounded body prefix.
fn embed_text(doc: &IndexDoc, max_chars: usize) -> String {
    let body: String = doc.body.chars().take(max_chars).collect();
    format!("{}\n{}", doc.title, body)
}

/// Embed every document into a store, in input order.
///
/// Pure and embedder-agnostic, so the CLI's aggregation is testable with the hash embedder
/// without a model download.
pub fn build_store(
    docs: &[IndexDoc],
    embedder: &dyn Embedder,
    max_chars: usize,
) -> Result<VectorStore, search_embed::StoreError> {
    let mut store = VectorStore::new(embedder.dim());
    // Batched: one inference per document measured 31.5 docs/s against 98.1 docs/s for batches
    // of 32 (BGE-small, 2026-10-28). A 1.1M-document backfill is 10 h instead of 31 h.
    let texts: Vec<String> = docs
        .iter()
        .map(|d| embed_text(d, max_chars))
        .collect();
    let vectors = embedder.embed_batch(&texts);
    if vectors.len() != docs.len() {
        return Err(search_embed::StoreError::Format(format!(
            "embedder returned {} vectors for {} documents",
            vectors.len(),
            docs.len()
        )));
    }
    for (doc, v) in docs.iter().zip(vectors) {
        store.insert(doc.doc_id.clone(), doc.source_id.clone(), v.v)?;
    }
    store.sort();
    Ok(store)
}

/// The configured embedder: real BGE when the build has it, otherwise the lexical stand-in.
fn make_embedder(kind: &str) -> Result<Box<dyn Embedder>, String> {
    match kind {
        "hash" => Ok(Box::new(HashEmbedder::new(256))),
        "bge" => {
            #[cfg(feature = "bge")]
            {
                let cache = std::env::var("SEARCH_MODEL_CACHE")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| PathBuf::from("models/fastembed"));
                search_embed::bge::BgeEmbedder::with_cache_dir_and_progress(cache)
                    .map(|e| Box::new(e) as Box<dyn Embedder>)
                    .map_err(|e| format!("{e}"))
            }
            #[cfg(not(feature = "bge"))]
            {
                Err("this build has no `bge` feature: rebuild with `--features bge`, or pass \
                     `--embedder hash` for the lexical stand-in"
                    .to_string())
            }
        }
        other => Err(format!("unknown embedder `{other}` (expected `bge` or `hash`)")),
    }
}

/// Which embedder to use when the caller did not say: BGE when the build has it.
fn default_embedder() -> &'static str {
    if cfg!(feature = "bge") {
        "bge"
    } else {
        "hash"
    }
}

fn default_out() -> PathBuf {
    Config::from_env().data_dir.join("vectors.bin")
}

/// Run the subcommand; returns the process exit code.
pub async fn run(args: &[String]) -> ExitCode {
    let cfg = Config::from_env();
    let kind = flag_str(args, "--embedder").unwrap_or_else(|| default_embedder().to_string());
    let out = flag_str(args, "--out").map(PathBuf::from).unwrap_or_else(default_out);
    let limit = flag_usize(args, "--limit").unwrap_or(usize::MAX);
    let max_chars = flag_usize(args, "--max-chars").unwrap_or(DEFAULT_MAX_CHARS);

    let embedder = match make_embedder(&kind) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("embedder `{kind}` unavailable: {e}");
            return ExitCode::from(2);
        }
    };

    // Read-only: no writer, so this runs beside a crawl.
    let pipeline = match Pipeline::open_read_only(&cfg, false) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to open the index read-only: {e}");
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
    let docs: Vec<IndexDoc> = docs.into_iter().take(limit).collect();

    let started = Instant::now();
    // Embed in chunks with a checkpoint after each, so a run that takes hours cannot lose
    // everything to one bad document or an OOM kill. Measured 2026-10-29: shard-0's 310K documents
    // is ~6.5 h at 13.2 docs/s, and the first version held every vector in memory and wrote
    // `vectors.bin` only at the end — one interruption discarded the whole run.
    //
    // The checkpoint is the output file itself, replaced atomically, holding everything embedded
    // so far. A re-run resumes from the checkpoint's document count.
    let mut store = match VectorStore::load_if_present(&out) {
        Some(s) => {
            println!("  resuming from {} ({})", out.display(), s.len());
            s
        }
        None => VectorStore::new(embedder.dim()),
    };
    let done = store.len();
    let todo: Vec<&IndexDoc> = docs.iter().skip(done).collect();
    let total = docs.len();
    const CHECKPOINT_EVERY: usize = 5_000;
    let mut batch_start = 0usize;
    while batch_start < todo.len() {
        let end = (batch_start + CHECKPOINT_EVERY).min(todo.len());
        let chunk: Vec<IndexDoc> = todo[batch_start..end].iter().map(|d| (*d).clone()).collect();
        match build_store(&chunk, embedder.as_ref(), max_chars) {
            Ok(part) => store.extend_from(&part),
            Err(e) => {
                eprintln!("failed to embed: {e}");
                return ExitCode::from(1);
            }
        }
        if let Err(e) = store.save(&out) {
            eprintln!("checkpoint write failed for {}: {e}", out.display());
            return ExitCode::from(1);
        }
        batch_start = end;
        println!(
            "  {}/{} embedded ({:.1}%)",
            done + batch_start,
            total,
            100.0 * (done + batch_start) as f64 / total.max(1) as f64
        );
    }
    if let Err(e) = store.save(&out) {
        eprintln!("failed to write {}: {e}", out.display());
        return ExitCode::from(1);
    }

    let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    println!("embed build (read-only): {} document(s)", store.len());
    println!("  embedder      : {kind} ({} dimensions)", store.dim());
    println!("  output        : {} ({:.1} MB)", out.display(), bytes as f64 / 1_048_576.0);
    println!("  body per doc  : {max_chars} characters");
    println!("  wall          : {:.1}s", started.elapsed().as_secs_f64());
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: &str, title: &str, body: &str) -> IndexDoc {
        IndexDoc::new(id, format!("res_{id}"), format!("https://example.com/{id}"), title, body)
    }

    #[test]
    fn the_title_is_always_embedded_and_the_body_is_bounded() {
        let long_body = "x".repeat(5000);
        let d = doc("d1", "Paged Attention", &long_body);
        let text = embed_text(&d, 100);
        assert!(text.starts_with("Paged Attention\n"), "{text}");
        assert_eq!(text.chars().count(), "Paged Attention\n".chars().count() + 100);
    }

    #[test]
    fn every_document_gets_a_sorted_entry_keyed_by_doc_id() {
        let docs = vec![
            doc("d2", "beta", "second document about queues"),
            doc("d1", "alpha", "first document about paging"),
        ];
        let store = build_store(&docs, &HashEmbedder::new(256), 200).unwrap();
        assert_eq!(store.len(), 2);
        assert_eq!(store.dim(), 256);
        assert!(store.get("d1").is_some());
        assert!(store.get("d2").is_some());

        // Sorted: the same input in a different order produces the same file.
        let mut reversed = docs.clone();
        reversed.reverse();
        let store2 = build_store(&reversed, &HashEmbedder::new(256), 200).unwrap();
        assert_eq!(store, store2, "the store must not depend on document order");
    }

    #[test]
    fn a_query_about_one_document_ranks_it_first() {
        // The end-to-end property the lane depends on, exercised with the deterministic
        // embedder so the test needs no model.
        let docs = vec![
            doc("d1", "Paged attention", "paged attention reduces KV cache memory in vLLM"),
            doc("d2", "Sourdough", "bread flour water salt starter bake"),
        ];
        let emb = HashEmbedder::new(256);
        let store = build_store(&docs, &emb, 400).unwrap();
        let q = emb.embed("paged attention KV cache").v;
        let hits = store.top_k(&q, 2);
        assert_eq!(hits[0].0, "d1", "{hits:?}");
        assert!(hits[0].1 > hits[1].1, "{hits:?}");
    }
}
