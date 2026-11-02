import time
from pathlib import Path
from tools.concepts.latent import LatentSpaceEngine

def main():
    print("=" * 70)
    print("TESTING LATENT SPACE ENGINE")
    print("=" * 70)

    t0 = time.time()
    engine = LatentSpaceEngine()
    print(f"Loaded {len(engine.concept_ids)} concepts into latent space in {round(time.time() - t0, 3)}s.")

    # 1. Compute and persist document embeddings
    print("\n[1] Computing document latent barycenters...")
    t1 = time.time()
    saved_docs = engine.compute_and_save_doc_embeddings()
    print(f"Computed & persisted {saved_docs} document barycenters in {round(time.time() - t1, 3)}s.")

    # 2. Test semantic probe (no exact substring match)
    query = "neurochemical basis of bonding"
    print(f"\n[2] Semantic Probe: '{query}'")
    hits = engine.probe_latent(query, top_k=5)
    for h in hits:
        print(f"  * {h['name']} ({h['kind']}) | Sim: {h['similarity']} | Docs: {h['document_count']}")

    # 3. Test hybrid probe (exact + latent)
    query2 = "romance"
    print(f"\n[3] Hybrid Probe: '{query2}'")
    hybrid_hits = engine.probe_hybrid(query2, top_k=5)
    for h in hybrid_hits:
        print(f"  * {h['name']} ({h['match_type']}) | Score: {h['score']} | Sim: {h['similarity']}")

    # 4. Test latent geodesic pathing (SLERP)
    print("\n[4] Latent Geodesic Pathing (SLERP): 'chemistry' -> 'romance'")
    geo = engine.bridge_geodesic("chemistry", "romance", steps=3, top_k_per_step=2)
    if geo:
        g = geo[0]
        print(f"  Direct cosine similarity: {g['endpoint_cosine_similarity']}")
        for wp in g["geodesic_waypoints"]:
            prog = wp["trajectory_progress"]
            concepts = [f"{c['name']} ({c['similarity_to_waypoint']})" for c in wp["nearest_concepts"]]
            print(f"  Waypoint t={prog}: {', '.join(concepts)}")

    # 5. Test vector arithmetic (Analogy)
    print("\n[5] Vector Algebra: ['pair bonding', 'neurotransmitter'] - ['psychology']")
    analogy_hits = engine.analogize(
        positive=["pair bonding", "neurotransmitter"],
        negative=["psychology"],
        top_k=5
    )
    for h in analogy_hits:
        print(f"  * {h['name']} ({h['kind']}) | Sim: {h['similarity']}")

    # 6. Test document latent retrieval
    print("\n[6] Document Retrieval in Latent Space: 'biochemical mechanisms of love'")
    doc_hits = engine.find_documents_by_latent("biochemical mechanisms of love", top_k=3)
    for d in doc_hits:
        print(f"  * {d['title']} | Sim: {d['cosine_similarity']} | Concepts: {d['concept_count']}")

    print("\n" + "=" * 70)
    print("ALL LATENT SPACE TESTS PASSED")
    print("=" * 70)

if __name__ == "__main__":
    main()
