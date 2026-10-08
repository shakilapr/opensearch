//! Ablation harness (plan Stage 2 exit gate).
//!
//! Builds a temp index from the committed fixture corpus, runs each lane combination
//! (bm25 · +dense · +sparse · source-routing ±global · cross-encoder on/off), and prints a
//! metrics table with the resource budget. See `docs/STAGE2_SPEC.md`.

fn main() {
    eprintln!("bench: not yet implemented");
}
