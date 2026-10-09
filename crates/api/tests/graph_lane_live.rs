//! Live check of the §9.4 lane against a real sidecar built by `agent0 graph build`.
//!
//! Ignored by default (it needs a corpus and its sidecar). Run:
//!
//! ```sh
//! SEARCH_GRAPH_SIDECAR=/var/lib/search/graph/entities.bin \
//!   cargo test -p search-api --test graph_lane_live -- --ignored --nocapture
//! ```
//!
//! It exists because the lane can be perfectly correct and still contribute nothing to a query —
//! the failure mode that matters here is silence, and the only way to see it is to ask the lane
//! directly and count what comes back.

use std::path::Path;

use search_api::graph_lane::GraphLaneSearcher;
use search_retrieval::LaneSearcher;

#[test]
#[ignore = "needs a built corpus and its graph sidecar"]
fn the_lane_resolves_real_entities_from_real_queries() {
    let path = std::env::var("SEARCH_GRAPH_SIDECAR")
        .expect("set SEARCH_GRAPH_SIDECAR to the entities.bin built by `agent0 graph build`");
    let lane = GraphLaneSearcher::from_path(Path::new(&path)).expect("sidecar must load");
    println!("entities in the lane: {}", lane.entity_count());

    let queries = [
        "vLLM",
        "paged attention",
        "vLLM paged attention KV cache",
        "transformer",
        "retrieval augmented generation",
        "vector database",
        "transformer architecture",
        "vector database indexing",
        "an entirely unrelated phrase about gardening",
    ];
    let mut any_hit = 0usize;
    for query in queries {
        let hits = lane.search(query, 10, None).expect("search must not fail");
        if !hits.is_empty() {
            any_hit += 1;
        }
        let ids: Vec<&str> = hits.iter().take(3).map(|h| h.doc_id.as_str()).collect();
        println!(
            "{query:?} -> seeds={} {} hit(s) {ids:?}",
            lane.seed_count(query),
            hits.len()
        );
    }
    assert!(
        any_hit > 0,
        "a lane over a {}-entity graph resolved no query at all: the join or the seeding is broken",
        lane.entity_count()
    );
}
