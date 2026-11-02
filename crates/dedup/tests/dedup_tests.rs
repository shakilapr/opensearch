//! Stage-5 exit-gate tests for the dedup crate (spec `docs/specs/STAGE5_SPEC.md`).
//!
//! A set of >=3 reworded documents must cluster as near-duplicates while a genuinely distinct
//! document stays out — verified both by SimHash Hamming distance and by MinHash/LSH banding.

use search_dedup::{lsh_bands, shingles, MinHash, NearDupIndex, SimHash64};
use std::collections::BTreeSet;

/// SimHash over the word tokens of a document.
fn fp(text: &str) -> SimHash64 {
    SimHash64::of_tokens(text.split_whitespace())
}

#[test]
fn reworded_documents_cluster_and_distinct_document_stays_out() {
    let a = "the quick brown fox jumps over the lazy dog in a rustic village near the river bank";
    let b = "the quick brown fox jumps over the lazy dog in a quiet village near the river bank";
    let c = "the quick brown fox jumps over the lazy dog in a rustic village near the stream bank";
    // Disjoint vocabulary and topic.
    let d = "quantum chromodynamics lattice gauge theory renormalization group asymptotically free coupling";

    let (fa, fb, fc, fd) = (fp(a), fp(b), fp(c), fp(d));

    // --- SimHash Hamming threshold -----------------------------------------------
    const MAX_HAMMING: u32 = 16;
    for (x, y, name) in [(fa, fb, "a-b"), (fa, fc, "a-c"), (fb, fc, "b-c")] {
        assert!(
            x.hamming(&y) <= MAX_HAMMING,
            "reworded pair {name} hamming {} exceeded {MAX_HAMMING}",
            x.hamming(&y)
        );
    }
    assert!(
        fa.hamming(&fd) > MAX_HAMMING,
        "distinct doc must exceed the threshold (hamming {})",
        fa.hamming(&fd)
    );

    // --- NearDupIndex cluster ----------------------------------------------------
    let mut index = NearDupIndex::new(4, 16, MAX_HAMMING);
    index.insert("doc-a", fa);
    index.insert("doc-b", fb);
    index.insert("doc-c", fc);

    let cluster = index.candidates(&fa);
    assert!(
        cluster.len() >= 3,
        "the three reworded docs form one cluster (got {cluster:?})"
    );
    assert_eq!(
        cluster,
        vec!["doc-a".to_string(), "doc-b".to_string(), "doc-c".to_string()],
        "candidate order must be deterministic"
    );
    assert!(index.is_near_dup(&fb), "a reworded doc is a near-dup");
    assert!(
        !index.is_near_dup(&fd),
        "the distinct doc must not be flagged as a near-dup"
    );

    // --- MinHash + LSH banding ---------------------------------------------------
    let mh = MinHash::new(32);
    let sig_a = mh.signature(&shingles(a, 2));
    let sig_b = mh.signature(&shingles(b, 2));
    let sig_c = mh.signature(&shingles(c, 2));
    let sig_d = mh.signature(&shingles(d, 2));

    let bands_a: BTreeSet<u64> = lsh_bands(&sig_a, 16).into_iter().collect();
    let bands_b: BTreeSet<u64> = lsh_bands(&sig_b, 16).into_iter().collect();
    let bands_c: BTreeSet<u64> = lsh_bands(&sig_c, 16).into_iter().collect();
    let bands_d: BTreeSet<u64> = lsh_bands(&sig_d, 16).into_iter().collect();

    assert!(!bands_a.is_disjoint(&bands_b));
    assert!(!bands_a.is_disjoint(&bands_c));
    assert!(!bands_b.is_disjoint(&bands_c));
    assert!(
        bands_a.is_disjoint(&bands_d),
        "the distinct document shares no LSH band"
    );
}

#[test]
fn candidate_order_is_stable_across_insertion_order() {
    let docs = ["m", "a", "z", "b"];
    let fps: Vec<SimHash64> = docs.iter().map(|s| fp(s)).collect();

    let mut forward = NearDupIndex::new(4, 16, 64);
    for (id, f) in docs.iter().zip(&fps) {
        forward.insert(*id, *f);
    }
    let mut backward = NearDupIndex::new(4, 16, 64);
    for (id, f) in docs.iter().zip(&fps).rev() {
        backward.insert(*id, *f);
    }

    // Querying with a doc's own fingerprint yields a stable, sorted candidate list
    // independent of insertion order.
    assert_eq!(forward.candidates(&fps[0]), backward.candidates(&fps[0]));
    let mut sorted = forward.candidates(&fps[0]);
    let check = sorted.clone();
    sorted.sort();
    assert_eq!(sorted, check, "candidates are always sorted");
}
