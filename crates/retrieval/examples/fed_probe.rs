use search_retrieval::LaneSearcher;
fn main() {
    let dirs: Vec<std::path::PathBuf> = (0..4).map(|s| std::path::PathBuf::from(format!("/var/lib/search-fleet/shard-{s}"))).collect();
    let lane = search_retrieval::concept::FederatedConceptLane::from_dirs(&dirs);
    let Some(lane) = lane else { println!("NO LANE"); return; };
    println!("shards={} dirs={}", lane.shard_count(), dirs.len());
    let q = "chemical reactions that affect romance";
    let t = std::time::Instant::now();
    let h = lane.search(q, 10, None).unwrap();
    println!("{} hits in {:?}", h.len(), t.elapsed());
    for x in h.iter().take(4) { println!("   {:.3} {}", x.score, x.doc_id); }
    // and what the dimensions do
    let l = search_retrieval::concept::ConceptLane::new("/var/lib/search-fleet/shard-1/meta/meta.sqlite");
    let c = l.concepts_for(q).unwrap();
    println!("concepts: {c:?}");
}
