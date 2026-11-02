# Stage 4 Spec — Agent navigation

Frozen interfaces for Stage 4. Depends on `core` + `retrieval` + `graph` + `rerank` (all
present). Do not redesign.

## Goal

Difficult ambiguous / multi-hop queries improve over one-shot retrieval; simple queries stay
in the fast path; **no agent (LLM) call happens when rules/policy suffice**.

## New crate

| crate dir     | package        | owns                                              |
|---------------|----------------|---------------------------------------------------|
| `crates/nav`  | `search-nav`   | SearchState, action frontier, beam, evidence joins, policy hierarchy, provider adapters |

## Frozen interfaces

```rust
pub enum Action {
    Retrieve { query: String },
    ExpandGraph { entity_id: String },
    Refresh { url: String },
    FollowLink { doc_id: String },
    Rerank,
    VerifyJoin { key: String },
    Widen,
}

/// Exact visited / action-key set gives cycle detection.
pub struct SearchState {
    pub query: String,
    pub visited_docs: BTreeSet<String>,
    pub action_keys: BTreeSet<String>,     // canonical key per action; re-issuing is a no-op
    pub hops: u8,
    pub results: Vec<String>,              // doc ids, best-first
}
impl SearchState {
    pub fn new(query: impl Into<String>) -> Self;
    pub fn key_of(action: &Action) -> String;   // stable canonical key
    pub fn seen(&self, action: &Action) -> bool;
}

pub struct SearchAction { pub action: Action, pub expected_gain: f32, pub cost: f32, pub admissible: bool }

pub enum StopReason { Solved, NoActions, Deadline, Budget, Plateau }
pub struct NavOutcome { pub state: SearchState, pub stop: StopReason, pub trace: Vec<Step> }
pub struct Step { pub action: Action, pub reason: String, pub used_policy: PolicyTier }

/// Policy hierarchy (engine-arch §12.1): cheapest sufficient tier wins.
pub enum PolicyTier { Rules, Classifier, LocalModel, Llm }
pub trait SearchPolicy: Send + Sync {
    fn tier(&self) -> PolicyTier;
    fn choose(&self, state: &SearchState, actions: &[SearchAction]) -> Option<SearchAction>;
}
pub struct RulesPolicy;        // deterministic heuristics; never needs the network
pub struct AlgorithmPolicy;    // best-first by expected_gain/cost
pub struct CallerAgentPolicy;  // the calling agent supplies the choice (structured state only)

/// Execution is pluggable so navigation is testable without a live index.
pub trait SearchExecutor: Send + Sync {
    fn execute(&mut self, action: &Action, state: &mut SearchState, ctx: &mut NavContext) -> Result<(), NavError>;
    fn candidates(&self, state: &SearchState, hints: &[String]) -> Vec<SearchAction>;
}

pub struct Budget { pub max_actions: u32, pub max_ms: u64, pub max_cost: f32,
                    pub used_actions: u32, pub used_ms: u64, pub used_cost: f32 }
impl Budget { pub fn exhausted(&self) -> bool; pub fn can_afford(&self, cost: f32) -> bool; }

pub struct NavConfig { pub beam: usize, pub max_hops: u8, pub reserve_per_interpretation: usize,
                       pub plateau_patience: u8 }
pub struct Navigator { /* policy chain, executor, budget, config */ }
impl Navigator {
    pub fn new(policies: Vec<Box<dyn SearchPolicy>>, executor: Box<dyn SearchExecutor>, budget: Budget, config: NavConfig) -> Self;
    /// Best-first traversal; escalates tiers only when the cheaper tier returns None AND an
    /// important constraint is uncovered AND predicted gain > cost AND the action is
    /// admissible AND the budget allows. Logs each Step with the tier used.
    pub fn run(&mut self, query: impl Into<String>) -> NavOutcome;
}

pub fn plan_budget(query: &str, config: &NavConfig) -> Budget;  // simple queries get a tight budget

/// Multi-hop: bind candidate identities, then retrieve dependent units sharing the join key
/// with a compatible time order. Returns the join key used, or None when unbound.
pub fn evidence_join(keys_a: &[String], keys_b: &[String], order_ok: bool) -> Option<String>;
```

Beam search: when a query is ambiguous, keep up to `beam` (2–4) partial interpretations, with
a reserved slot per credible interpretation, instead of committing to one (engine §18).

Hard invariants (test them): a re-issued action (same canonical key) is never executed twice;
`Budget::exhausted()` stops navigation with `StopReason::Budget`; the LLM tier is **never**
reached for a simple query; `PolicyTier` escalates in order Rules → Classifier → LocalModel → Llm.

## Provider adapters

`OpenAI`, `Claude`, `DeepSeek`, `GLM`, `LocalPolicy` implement `SearchPolicy` behind a
`ProviderPolicy` that takes a `dyn ModelClient` (a trait with `fn complete(&self, prompt) ->
Result<String>`). No API keys; the client is injected. The model sees **only structured
state + allowed action IDs — never a raw URL**. Default builds use `RulesPolicy` +
`AlgorithmPolicy`; provider adapters must compile without credentials and must be testable
with a fake `ModelClient`.

## Exit gate

Difficult ambiguous/multi-hop queries improve vs one-shot retrieval; simple queries stay in
the fast path; no LLM call happens when rules/policy suffice — all demonstrated by hermetic
tests with a fake executor and a fake model client that counts calls (must be 0 for simple
queries).
