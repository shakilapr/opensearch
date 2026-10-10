//! Live check: does the kitani model actually produce usable query plans?
//!
//! Ignored by default (it spends tokens, and the first run per distinct prompt is a real call).
//! Run:
//!   cargo test -p search-organize --test queryplan_live -- --ignored --nocapture
//!
//! It asserts the two things a plan must do to be worth wiring in: raise the precision of a
//! multi-word question (phrases/required terms rather than a bag of words), and name the entities
//! the question implies so the §9.4 graph lane has something to activate from.

use search_organize::{LlmClient, LlmConfig};

#[test]
#[ignore = "makes real model calls (cached after the first run)"]
fn the_planner_turns_questions_into_probes() {
    let cfg = LlmConfig::from_env().expect("set KITANI_API_KEY to run this");
    let cache = std::env::var("SEARCH_LLM_CACHE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/var/lib/search/llm-cache"));
    let client = LlmClient::new(cfg, cache, 4).expect("client");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let questions = [
        "how does flash attention reduce memory",
        "what is paged attention in vLLM",
        "kv cache offloading techniques",
    ];
    let mut with_entities = 0usize;
    for q in questions {
        let plan = rt.block_on(client.plan_query(q, &[])).expect("call").expect("a usable plan");
        println!("\nQ: {q}\n  query    : {}\n  entities : {:?}\n  phrases  : {:?}\n  negatives: {:?}",
                 plan.query, plan.entities, plan.phrases, plan.negatives);
        if !plan.entities.is_empty() {
            with_entities += 1;
        }
    }
    // A planner that never names an entity cannot seed the graph lane, and one that returns the
    // question unchanged has not deconstructed anything.
    assert!(with_entities > 0, "the planner named no entity for any question");
    println!("\ncache: {:?}", client.cache_stats());
}
