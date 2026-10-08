//! Hermetic Stage-4 tests: fake executor + fake `ModelClient` that counts calls.
//!
//! No network, no live index, no credentials.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use search_nav::{
    evidence_join, plan_budget, Action, Budget, ModelClient, NavConfig, NavContext, NavError, Navigator, PolicyTier, Provider,
 ProviderPolicy, RulesPolicy, SearchAction, SearchExecutor, SearchPolicy, SearchState,
 StopReason, AlgorithmPolicy,
};

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

type Plan = Box<dyn Fn(&SearchState) -> Vec<SearchAction> + Send + Sync>;
type Exec = Box<dyn Fn(&Action, &mut SearchState, &mut NavContext) + Send + Sync>;

/// A scripted executor. `plan` proposes candidates from the state; `exec` mutates state/ctx.
struct ScriptExec {
    plan: Plan,
    exec: Exec,
    calls: Arc<Mutex<Vec<String>>>,
}

impl ScriptExec {
    fn new(plan: Plan, exec: Exec) -> Self {
        Self {
            plan,
            exec,
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn calls(&self) -> Arc<Mutex<Vec<String>>> {
        self.calls.clone()
    }
}

impl SearchExecutor for ScriptExec {
    fn execute(
        &mut self,
        action: &Action,
        state: &mut SearchState,
        ctx: &mut NavContext,
    ) -> Result<(), NavError> {
        self.calls
            .lock()
            .unwrap()
            .push(SearchState::key_of(action));
        (self.exec)(action, state, ctx);
        Ok(())
    }

    fn candidates(&self, state: &SearchState, _hints: &[String]) -> Vec<SearchAction> {
        (self.plan)(state)
    }
}

/// A fake model client that counts calls and records prompts.
struct CountingClient {
    calls: Arc<AtomicUsize>,
    prompts: Arc<Mutex<Vec<String>>>,
    reply: String,
}

impl CountingClient {
    fn new(reply: &str) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            prompts: Arc::new(Mutex::new(Vec::new())),
            reply: reply.to_string(),
        }
    }
    fn calls(&self) -> Arc<AtomicUsize> {
        self.calls.clone()
    }
    fn prompts(&self) -> Arc<Mutex<Vec<String>>> {
        self.prompts.clone()
    }
}

impl ModelClient for CountingClient {
    fn complete(&self, prompt: &str) -> Result<String, NavError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.prompts.lock().unwrap().push(prompt.to_string());
        Ok(self.reply.clone())
    }
}

fn ra(action: Action, gain: f32, cost: f32) -> SearchAction {
    SearchAction::new(action, gain, cost)
}

// ---------------------------------------------------------------------------
// Executors used by several tests
// ---------------------------------------------------------------------------

/// Simple: one obvious retrieve; delivers one doc and signals solved.
fn simple_exec() -> ScriptExec {
    ScriptExec::new(
        Box::new(|s: &SearchState| {
            if s.results.is_empty() {
                vec![ra(Action::Retrieve { query: s.query.clone() }, 1.0, 0.1)]
            } else {
                vec![]
            }
        }),
        Box::new(|a: &Action, s: &mut SearchState, ctx: &mut NavContext| {
            if let Action::Retrieve { .. } = a {
                s.add_result("d0");
                ctx.mark_solved();
            }
        }),
    )
}

/// Ambiguous "apple": two comparable entity interpretations; following the fruit reading
/// surfaces a doc a single one-shot retrieve never sees.
fn ambiguous_exec() -> ScriptExec {
    ScriptExec::new(
        Box::new(|s: &SearchState| {
            if s.visited_docs.contains("the_doc_one_shot_misses") {
                return vec![];
            }
            let keys = &s.action_keys;
            let expanded = keys.iter().any(|k| k.starts_with("expand_graph"));
            if !expanded {
                vec![
                    ra(Action::ExpandGraph { entity_id: "apple_fruit".into() }, 0.9, 0.1),
                    ra(Action::ExpandGraph { entity_id: "apple_inc".into() }, 0.88, 0.1),
                ]
            } else if !keys.iter().any(|k| k.starts_with("follow_link")) {
                let mut v = Vec::new();
                if keys.contains("expand_graph:apple_fruit") {
                    v.push(ra(
                        Action::FollowLink { doc_id: "the_doc_one_shot_misses".into() },
                        0.8,
                        0.1,
                    ));
                }
                if keys.contains("expand_graph:apple_inc") {
                    v.push(ra(Action::FollowLink { doc_id: "d_inc".into() }, 0.8, 0.1));
                }
                v
            } else {
                vec![]
            }
        }),
        Box::new(|a: &Action, s: &mut SearchState, _ctx: &mut NavContext| {
            match a {
                Action::FollowLink { doc_id } => {
                    s.add_result(doc_id.clone());
                }
                Action::Retrieve { .. } => {
                    s.add_result("d0");
                }
                _ => {}
            }
        }),
    )
}

/// Always offers the same action; used for the re-issue test.
fn sticky_exec() -> ScriptExec {
    ScriptExec::new(
        Box::new(|s: &SearchState| {
            vec![ra(Action::Retrieve { query: s.query.clone() }, 1.0, 0.1)]
        }),
        Box::new(|_: &Action, _: &mut SearchState, _: &mut NavContext| {}),
    )
}

/// Offers a fresh distinct retrieve every hop, never solved (for budget/deadline tests).
fn endless_exec(ms_per_step: u64) -> ScriptExec {
    ScriptExec::new(
        Box::new(|s: &SearchState| {
            vec![ra(
                Action::Retrieve { query: format!("{}#{}", s.query, s.hops) },
                1.0,
                0.1,
            )]
        }),
        Box::new(move |a: &Action, s: &mut SearchState, ctx: &mut NavContext| {
            ctx.charge_ms(ms_per_step);
            if let Action::Retrieve { query } = a {
                s.add_result(format!("doc:{query}"));
            }
        }),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn reissued_action_not_executed_twice() {
    let exec = sticky_exec();
    let calls = exec.calls();
    let mut nav = Navigator::new(
        vec![Box::new(RulesPolicy)],
        Box::new(exec),
        Budget::unbounded(),
        NavConfig::default(),
    );
    let out = nav.run("rust ownership");
    assert_eq!(calls.lock().unwrap().len(), 1, "same key dispatched exactly once");
    assert_eq!(out.stop, StopReason::NoActions);
    assert!(out.trace.len() == 1);
}

#[test]
fn budget_exhaustion_stops_with_budget() {
    let mut nav = Navigator::new(
        vec![Box::new(RulesPolicy)],
        Box::new(endless_exec(0)),
        Budget::new(2, u64::MAX, f32::MAX),
        NavConfig::default(),
    );
    let out = nav.run("deep topic");
    assert_eq!(out.stop, StopReason::Budget);
    assert!(out.trace.len() <= 2, "never more than max_actions steps");
}

#[test]
fn simple_query_uses_rules_and_zero_model_calls() {
    let client = CountingClient::new("A0");
    let calls = client.calls();
    let cfg = NavConfig::default();
    let mut nav = Navigator::new(
        vec![
            Box::new(RulesPolicy),
            Box::new(ProviderPolicy::new(Provider::OpenAI, Arc::new(client))),
        ],
        Box::new(simple_exec()),
        plan_budget("rust ownership", &cfg),
        cfg,
    );
    let out = nav.run("rust ownership");
    assert_eq!(out.stop, StopReason::Solved);
    assert!(!out.trace.is_empty());
    assert_eq!(out.trace[0].used_policy, PolicyTier::Rules, "rules decides the simple query");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "model client must not be called");
}

#[test]
fn ambiguous_query_escalates_and_improves_vs_one_shot() {
    // One-shot: a single retrieve.
    let mut one = ambiguous_exec();
    let mut st = SearchState::new("apple problems");
    let mut ctx = NavContext::new(Budget::unbounded());
    one.execute(
        &Action::Retrieve { query: "apple problems".into() },
        &mut st,
        &mut ctx,
    )
    .unwrap();
    assert!(!st.results.iter().any(|d| d == "the_doc_one_shot_misses"));

    let cfg = NavConfig { plateau_patience: 4, ..NavConfig::default() };
    let mut nav = Navigator::new(
        vec![Box::new(RulesPolicy), Box::new(AlgorithmPolicy)],
        Box::new(ambiguous_exec()),
        Budget::unbounded(),
        cfg,
    );
    let out = nav.run("apple problems");

    assert!(
        out.state.results.iter().any(|d| d == "the_doc_one_shot_misses"),
        "navigation finds the doc one-shot misses: {:?}",
        out.state.results
    );
    assert!(
        out.trace.iter().any(|s| s.used_policy == PolicyTier::Classifier),
        "escalated past Rules to the algorithmic tier"
    );
    assert!(
        out.trace.iter().all(|s| s.used_policy >= PolicyTier::Rules),
        "tier order respected"
    );
}

#[test]
fn policy_escalation_order_and_cheapest_wins() {
    assert!(PolicyTier::Rules < PolicyTier::Classifier);
    assert!(PolicyTier::Classifier < PolicyTier::LocalModel);
    assert!(PolicyTier::LocalModel < PolicyTier::Llm);

    // Policies supplied out of order; the navigator must still consult Rules first.
    let client = CountingClient::new("A0");
    let calls = client.calls();
    let mut nav = Navigator::new(
        vec![
            Box::new(ProviderPolicy::new(Provider::OpenAI, Arc::new(client))),
            Box::new(RulesPolicy),
        ],
        Box::new(simple_exec()),
        Budget::unbounded(),
        NavConfig::default(),
    );
    let out = nav.run("rust ownership");
    assert_eq!(out.trace[0].used_policy, PolicyTier::Rules);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    // RulesPolicy declines the ambiguous set (two comparable interpretations).
    let state = SearchState::new("apple problems");
    let actions = vec![
        ra(Action::ExpandGraph { entity_id: "apple_fruit".into() }, 0.9, 0.1),
        ra(Action::ExpandGraph { entity_id: "apple_inc".into() }, 0.88, 0.1),
    ];
    assert!(RulesPolicy.choose(&state, &actions).is_none());
    assert!(AlgorithmPolicy.choose(&state, &actions).is_some());
}

#[test]
fn llm_tier_reached_only_when_cheaper_tiers_abstain() {
    let client = CountingClient::new("A0");
    let calls = client.calls();
    let exec = ScriptExec::new(
        Box::new(|s: &SearchState| {
            if s.hops == 0 {
                vec![ra(Action::Refresh { url: "https://example.com/a".into() }, 0.6, 0.1)]
            } else {
                vec![]
            }
        }),
        Box::new(|_: &Action, _: &mut SearchState, _: &mut NavContext| {}),
    );
    let mut nav = Navigator::new(
        vec![
            Box::new(RulesPolicy),
            Box::new(ProviderPolicy::new(Provider::Local, Arc::new(client))),
        ],
        Box::new(exec),
        Budget::unbounded(),
        NavConfig::default(),
    );
    let out = nav.run("refresh the source");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the model tier was reached once");
    assert_eq!(out.trace[0].used_policy, PolicyTier::LocalModel);
    assert!(matches!(out.trace[0].action, Action::Refresh { .. }));
}

#[test]
fn provider_prompt_contains_no_raw_url() {
    let secret = "https://example.com/secret";
    let raw = "https://raw.example.com/a";
    let client = Arc::new(CountingClient::new("A0"));
    let prompts = client.prompts();
    let policy = ProviderPolicy::new(Provider::OpenAI, client);

    let state = SearchState::new(format!("find {secret} info"));
    let actions = vec![ra(Action::Refresh { url: raw.into() }, 0.6, 0.1)];
    let picked = policy.choose(&state, &actions).expect("model returns an id");

    let prompts = prompts.lock().unwrap();
    assert_eq!(prompts.len(), 1);
    let p = &prompts[0];
    assert!(!p.contains("://"), "no scheme separator in the prompt");
    assert!(!p.contains(secret), "raw query URL leaked");
    assert!(!p.contains(raw), "raw action URL leaked");
    assert!(!p.contains("example.com"), "no host leaked");
    assert!(p.contains("A0"), "allowed action id present");
    assert!(p.contains("refresh"), "action kind present");
    assert!(matches!(picked.action, Action::Refresh { .. }));
}

#[test]
fn beam_reserves_a_slot_per_interpretation() {
    fn entities_with_beam(beam: usize) -> HashSet<String> {
        let cfg = NavConfig { beam, plateau_patience: 4, ..NavConfig::default() };
        let mut nav = Navigator::new(
            vec![Box::new(RulesPolicy), Box::new(AlgorithmPolicy)],
            Box::new(ambiguous_exec()),
            Budget::unbounded(),
            cfg,
        );
        let out = nav.run("apple problems");
        out.trace
            .iter()
            .filter_map(|s| match &s.action {
                Action::ExpandGraph { entity_id } => Some(entity_id.clone()),
                _ => None,
            })
            .collect()
    }

    let b1 = entities_with_beam(1);
    assert_eq!(b1.len(), 1, "beam=1 commits to a single interpretation");

    let b3 = entities_with_beam(3);
    assert_eq!(b3.len(), 2, "beam=3 keeps both credible interpretations");
    assert!(b3.contains("apple_fruit") && b3.contains("apple_inc"));
}

#[test]
fn evidence_join_positive_negative_and_order_sensitive() {
    let a = vec!["person:1".to_string(), "person:2".to_string()];
    let b = vec!["org:9".to_string(), "person:2".to_string()];
    assert_eq!(evidence_join(&a, &b, true), Some("person:2".to_string()));
    assert_eq!(evidence_join(&a, &b, false), None, "order incompatible -> unbound");

    let c = vec!["x".to_string()];
    assert_eq!(evidence_join(&a, &c, true), None, "disjoint -> None");
    assert_eq!(evidence_join(&[], &b, true), None, "empty side -> None");
}

#[test]
fn time_budget_stops_navigation() {
    let cfg = NavConfig { max_hops: 20, plateau_patience: 0, ..NavConfig::default() };
    // The executor advances a virtual clock 100 ms per step; the budget caps wall time at
    // 150 ms, so navigation must stop after at most two steps.
    let mut nav = Navigator::new(
        vec![Box::new(RulesPolicy)],
        Box::new(endless_exec(100)),
        Budget::new(u32::MAX, 150, f32::MAX),
        cfg,
    );
    let out = nav.run("slow query");
    assert_eq!(out.stop, StopReason::Budget, "time cap reached: {:?}", out.stop);
    assert!(out.trace.len() <= 2);
}
