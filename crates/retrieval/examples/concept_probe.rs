use search_retrieval::LaneSearcher;
fn main() {
    let lane = search_retrieval::concept::ConceptLane::new(
        std::path::Path::new("/tmp/cx2/meta/meta.sqlite"));
    for q in ["chemical reactions that affect romance",
              "how oxytocin builds attachment",
              "romantic love and hormones"] {
        let concepts = lane.concepts_for(q).unwrap();
        let hits = lane.search(q, 5, None).unwrap();
        println!("\nQ: {q}");
        println!("  concepts: {:?}", concepts);
        for h in hits.iter().take(3) {
            println!("    {:.3}  {}", h.score, h.doc_id);
        }
    }
}
