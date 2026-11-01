//! Diagnostic: how many of the vocabulary's features actually fire?
//!
//! ```text
//! cargo run --release -p search-features --example occupancy -- <vectors.bin> [features] [active]
//! ```
//!
//! Prints occupancy for (a) a data-seeded encoder with no training and (b) the same encoder after
//! one pass, so a collapse can be attributed to the initialisation or to the optimiser instead of
//! being guessed at.

use search_embed::VectorStore;
use search_features::encoder::{Encoder, TrainOptions};

fn occupancy(enc: &Encoder, samples: &[Vec<f32>]) -> usize {
    let mut live = vec![false; enc.features()];
    for s in samples {
        if let Ok(hits) = enc.encode(s, enc.active()) {
            for h in hits {
                live[h.feature as usize] = true;
            }
        }
    }
    live.iter().filter(|x| **x).count()
}

/// Mean squared reconstruction error of the tied decoder, over `samples`.
fn recon_err(enc: &Encoder, samples: &[Vec<f32>]) -> f32 {
    let mut total = 0.0f32;
    for x in samples {
        let hits = enc.code_topk(x, enc.active()).unwrap_or_default();
        let mut recon = vec![0.0f32; x.len()];
        for h in &hits {
            let row = enc.row(h.feature as usize);
            for (r, w) in recon.iter_mut().zip(row.iter()) {
                *r += h.activation * w;
            }
        }
        total += recon.iter().zip(x.iter()).map(|(r, v)| (r - v).powi(2)).sum::<f32>();
    }
    total / samples.len() as f32
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).cloned().unwrap_or_else(|| "vectors.bin".into());
    let features = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(4096);
    let active = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(16);
    let store = VectorStore::load(std::path::Path::new(&path)).expect("load vectors");
    let samples: Vec<Vec<f32>> = store.entries().iter().map(|e| e.v.clone()).collect();
    println!("{path}: {} documents, {} dims", samples.len(), store.dim());

    let seeded = Encoder::from_samples(&samples, store.dim(), features, active, 7);
    println!("data-seeded, untrained : {} of {} features live, recon {:.4}", occupancy(&seeded, &samples), features, recon_err(&seeded, &samples));

    let random = Encoder::random(store.dim(), features, active, 7);
    println!("random, untrained      : {} of {} features live", occupancy(&random, &samples), features);

    let opts = TrainOptions {
        features,
        active,
        epochs: 1,
        batch: 2048,
        lr: 0.01,
        momentum: 0.9,
        seed: 7,
        log_every: 0,
        resample_every: 20_000,
        train_bias: false,
    };
    let trained = search_features::train(&samples, store.dim(), &opts).expect("train");
    println!("trained, 1 epoch       : {} of {} features live, recon {:.4}", occupancy(&trained, &samples), features, recon_err(&trained, &samples));
}