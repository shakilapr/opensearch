//! `bench` — Stage-2 ablation CLI.
//!
//! Builds a temp index from `benchmarks/corpus`, runs every lane combination over
//! `benchmarks/queries.example.jsonl`, prints the metrics table + resource budget, and writes
//! `benchmarks/results.json`. All of the work lives in the `search_bench` library so the same
//! code path is exercised by `cargo test -p search-bench`.

use std::path::PathBuf;

use search_bench::{render_table, run, Config, EmbedderKind};

fn usage() -> String {
    "usage: bench [--corpus DIR] [--queries FILE] [--out FILE] [--k N] [--dim N] \
     [--embedder hash|bge] [--ce-depth N]\n\n\
     embedder:\n  hash  deterministic hashed bag-of-words (default; lexical, not semantic)\n  \
     bge   real BGE-small-en-v1.5, 384-d (requires a build with --features bge)\n\n\
     defaults:\n"
        .to_string()
        + &format!(
            "  corpus : {}\n  queries: {}\n  out    : {}\n  embedder: hash\n  dim    : 1024 (hash embedder only)\n",
            search_bench::DEFAULT_CORPUS_DIR,
            search_bench::DEFAULT_QUERIES,
            search_bench::DEFAULT_OUT,
        )
}

fn parse_args_from<I: Iterator<Item = String>>(args: I) -> Result<(Config, bool), String> {
    let mut cfg = Config::default();
    let mut per_case = false;
    let mut args = args;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--corpus" => cfg.corpus_dir = PathBuf::from(args.next().ok_or("--corpus needs a value")?),
            "--queries" => {
                cfg.queries_path = PathBuf::from(args.next().ok_or("--queries needs a value")?)
            }
            "--out" => cfg.out_path = PathBuf::from(args.next().ok_or("--out needs a value")?),
            "--k" => cfg.k = args.next().ok_or("--k needs a value")?.parse().map_err(|e| format!("--k: {e}"))?,
            "--dim" => cfg.dim = args.next().ok_or("--dim needs a value")?.parse().map_err(|e| format!("--dim: {e}"))?,
            "--embedder" => {
                let v = args.next().ok_or("--embedder needs a value")?;
                cfg.embedder = EmbedderKind::parse(&v).map_err(|e| format!("--embedder: {e}"))?;
            }
            "--ce-depth" => {
                cfg.ce_depth = args.next().ok_or("--ce-depth needs a value")?.parse().map_err(|e| format!("--ce-depth: {e}"))?
            }
            "-h" | "--help" => return Err(usage()),
            "--per-case" => per_case = true,
            other => return Err(format!("unknown argument `{other}`\n\n{}", usage())),
        }
    }
    Ok((cfg, per_case))
}

fn parse_args() -> Result<(Config, bool), String> {
    parse_args_from(std::env::args().skip(1))
}

fn main() {
    let (cfg, per_case) = match parse_args() {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };

    let report = match run(&cfg) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("bench failed: {e}");
            std::process::exit(1);
        }
    };

    println!("Stage-2 ablation harness (search-bench v{})", report.version);
    println!(
        "corpus: {} docs | queries: {} | k={} | embedder={} | embed_dim={} | ce_depth={}",
        report.budget.docs, report.budget.queries, report.budget.k, report.budget.embedder, report.budget.embed_dim, report.budget.ce_depth
    );
    println!();
    print!("{}", render_table(&report));
    println!();
    println!("resource budget:");
    println!(
        "  candidates/query  mean {:.1}  max {}",
        report.budget.mean_candidates, report.budget.max_candidates
    );
    println!(
        "  latency           see p50/p95 columns above (ms, whole-ablation per query)"
    );
    println!(
        "  peak RSS          {} KB (sampled from /proc/self/statm)",
        report.budget.peak_rss_kb
    );
    println!();
    if per_case {
        println!("per-case rank of first relevant doc (1 = top; 0 = none in top-k):");
        for c in &report.per_case {
            let mark = if c.hybrid_rank < c.bm25_rank || (c.bm25_rank == 0 && c.hybrid_rank > 0) {
                "hybrid wins"
            } else if c.hybrid_rank > c.bm25_rank || (c.hybrid_rank == 0 && c.bm25_rank > 0) {
                "bm25 wins"
            } else {
                "tie"
            };
            println!(
                "  {:<20} bm25={:<3} hybrid={:<3} {}",
                c.case_id, c.bm25_rank, c.hybrid_rank, mark
            );
        }
        println!();
    }
    println!("notes:");
    for n in &report.notes {
        println!("  - {n}");
    }
    println!();
    println!("wrote {}", cfg.out_path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<(Config, bool), String> {
        parse_args_from(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_to_hash_embedder() {
        let (cfg, per_case) = parse(&[]).unwrap();
        assert_eq!(cfg.embedder, EmbedderKind::Hash);
        assert!(!per_case);
    }

    #[test]
    fn selector_parses_embedder_bge() {
        let (cfg, _) = parse(&["--embedder", "bge"]).unwrap();
        assert_eq!(cfg.embedder, EmbedderKind::Bge);
        // and explicit hash
        let (cfg, _) = parse(&["--embedder", "hash"]).unwrap();
        assert_eq!(cfg.embedder, EmbedderKind::Hash);
    }

    #[test]
    fn selector_rejects_unknown_embedder() {
        let err = parse(&["--embedder", "potion"]).unwrap_err();
        assert!(err.contains("--embedder"), "error was: {err}");
        assert!(err.contains("hash"), "error was: {err}");
    }

    #[test]
    fn usage_mentions_embedder_and_bge() {
        let u = usage();
        assert!(u.contains("--embedder"), "usage was: {u}");
        assert!(u.contains("bge"), "usage was: {u}");
    }

    #[test]
    fn dim_and_embedder_combine() {
        let (cfg, _) = parse(&["--dim", "256", "--embedder", "bge"]).unwrap();
        assert_eq!(cfg.dim, 256);
        assert_eq!(cfg.embedder, EmbedderKind::Bge);
    }
}
