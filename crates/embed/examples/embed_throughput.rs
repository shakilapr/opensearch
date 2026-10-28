//! Throughput probe for the embedder: one inference per document (what `embed build` did
//! before batching) versus batched inference via `Embedder::embed_batch`.
//!
//! `cargo run --release -p search-embed --example embed_throughput --features bge`.
//! Without the `bge` feature the example compiles to a message, so a default workspace build
//! never needs ONNX Runtime just to build it.

#[cfg(feature = "bge")]
use std::time::Instant;

#[cfg(feature = "bge")]
use search_embed::bge::BgeEmbedder;
#[cfg(feature = "bge")]
use search_embed::Embedder;

#[cfg(feature = "bge")]
fn corpus(n: usize) -> Vec<String> {
    // ~2000 characters of prose, the shape `embed build` actually feeds the model.
    let base = "Retrieval systems combine lexical scoring with dense representations to \
                answer questions over a heterogeneous corpus. A passage about chemistry and \
                one about romantic attachment may share vocabulary without sharing meaning, \
                which is why a weighted concept layer earns its keep. ";
    (0..n)
        .map(|i| format!("{base}Document {i} discusses evaluation, latency and recall."))
        .collect()
}

#[cfg(not(feature = "bge"))]
fn main() {
    println!("build with --features bge to measure BGE throughput");
}

#[cfg(feature = "bge")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cache = std::env::var("SEARCH_MODEL_CACHE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/var/lib/search/fastembed"));
    let e = BgeEmbedder::with_cache_dir(cache)?;
    let docs = corpus(256);

    // Warm up so the first inference's graph construction is not counted.
    let _ = e.embed(&docs[0]);

    let t = Instant::now();
    for d in &docs {
        let _ = e.embed(d);
    }
    let one = t.elapsed();

    let t = Instant::now();
    for chunk in docs.chunks(32) {
        let _ = e.embed_batch(chunk)?;
    }
    let batch = t.elapsed();

    let n = docs.len() as f64;
    println!("documents          : {}", docs.len());
    println!(
        "one-at-a-time      : {:.2}s  {:.1} docs/s",
        one.as_secs_f64(),
        n / one.as_secs_f64()
    );
    println!(
        "batched (32)       : {:.2}s  {:.1} docs/s",
        batch.as_secs_f64(),
        n / batch.as_secs_f64()
    );
    println!(
        "speedup            : {:.1}x",
        one.as_secs_f64() / batch.as_secs_f64()
    );
    Ok(())
}
