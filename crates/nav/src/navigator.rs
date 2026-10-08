//! The navigator: best-first traversal, bounded beam, stopping, budget planning, joins.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::budget::Budget;
use crate::executor::{NavContext, SearchExecutor};
use crate::policy::{PolicyTier, SearchPolicy};
use crate::state::{group_by_key, Action, SearchAction, SearchState};

/// Why navigation stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The executor signalled the query is answered.
    Solved,
    /// No admissible, unseen action remained (or no policy would choose one).
    NoActions,
    /// The client deadline was reached.
    Deadline,
    /// A resource budget cap was reached.
    Budget,
    /// No new documents for `plateau_patience` rounds (a heuristic — engine-arch §17).
    Plateau,
}

/// One executed navigation step.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub action: Action,
    pub reason: String,
    pub used_policy: PolicyTier,
}

/// The result of a navigation run.
#[derive(Debug, Clone, PartialEq)]
pub struct NavOutcome {
    pub state: SearchState,
    pub stop: StopReason,
    pub trace: Vec<Step>,
}

/// Navigator tuning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavConfig {
    /// Beam width: how many partial interpretations to keep alive (engine §18, 2–4 typical).
    pub beam: usize,
    /// Maximum hops (executed actions along a path).
    pub max_hops: u8,
    /// Slots reserved per credible interpretation when expanding an ambiguous node.
    pub reserve_per_interpretation: usize,
    /// Stop with `Plateau` after this many rounds with no new documents (0 disables).
    pub plateau_patience: u8,
}

impl Default for NavConfig {
    fn default() -> Self {
        Self {
            beam: 3,
            max_hops: 6,
            reserve_per_interpretation: 1,
            plateau_patience: 3,
        }
    }
}

/// A node in the bounded beam: a partial state, its cumulative gain, and the interpretation
/// that produced it.
#[derive(Debug, Clone)]
struct Node {
    state: SearchState,
    score: f32,
    tag: String,
}

impl Node {
    fn root(query: &str) -> Self {
        Self {
            state: SearchState::new(query),
            score: 0.0,
            tag: String::new(),
        }
    }
}

fn cmp_node(a: &Node, b: &Node) -> Ordering {
    a.state
        .results
        .len()
        .cmp(&b.state.results.len())
        .then_with(|| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(Ordering::Equal)
        })
}

fn better(a: &Node, b: &Node) -> bool {
    cmp_node(a, b) == Ordering::Greater
}

/// Beam selection: keep up to `beam` children, taking them round-robin across interpretation
/// groups so no single reading can fill every slot (reserved slot per credible interpretation).
/// Per-group cap = `max(reserve, beam + 1 - num_groups)`.
fn select_beam(children: Vec<Node>, beam: usize, reserve: usize) -> Vec<Node> {
    let mut groups: Vec<(String, Vec<Node>)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for c in children {
        if let Some(&i) = index.get(&c.tag) {
            groups[i].1.push(c);
        } else {
            index.insert(c.tag.clone(), groups.len());
            groups.push((c.tag.clone(), vec![c]));
        }
    }
    for g in groups.iter_mut() {
        g.1.sort_by(|a, b| cmp_node(b, a));
    }
    groups.sort_by(|a, b| cmp_node(&b.1[0], &a.1[0]));

    let ng = groups.len();
    let cap = (beam + 1).saturating_sub(ng).max(reserve).max(1);
    let mut cursors = vec![0usize; ng];
    let mut out = Vec::new();
    loop {
        let mut progressed = false;
        for gi in 0..ng {
            if out.len() >= beam {
                break;
            }
            if cursors[gi] < groups[gi].1.len() && cursors[gi] < cap {
                out.push(groups[gi].1[cursors[gi]].clone());
                cursors[gi] += 1;
                progressed = true;
            }
        }
        if !progressed || out.len() >= beam {
            break;
        }
    }
    out
}

/// Best-first navigator over a policy chain and a pluggable executor.
pub struct Navigator {
    policies: Vec<Box<dyn SearchPolicy>>,
    executor: Box<dyn SearchExecutor>,
    budget: Budget,
    config: NavConfig,
}

/// Compute a budget for a query. Short, single-entity queries get a tight budget (few actions,
/// small cost) so they stay entirely in the fast path and never reach a model tier; complex /
/// multi-hop queries scale with `config.max_hops`.
pub fn plan_budget(query: &str, config: &NavConfig) -> Budget {
    let toks: Vec<&str> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect();
    let lc = query.to_lowercase();
    let multihop = toks.len() > 6
        || [
            " vs ", " versus ", " compare ", " then ", " between ", " who ", " founded ",
            " relationship ", " and ", " more than ", " after ", " before ",
        ]
        .iter()
        .any(|m| lc.contains(m));

    if !multihop && toks.len() <= 4 {
        Budget::new(2, 50, 1.0)
    } else {
        let actions = 4 + (config.max_hops as u32).saturating_mul(2);
        Budget::new(actions, 2_000, 10.0)
    }
}

/// Multi-hop evidence join: bind candidate identities, then retrieve dependent units sharing
/// the join key with a compatible time order. Returns the first key present in both lists when
/// `order_ok`, else `None`.
pub fn evidence_join(keys_a: &[String], keys_b: &[String], order_ok: bool) -> Option<String> {
    if !order_ok {
        return None;
    }
    keys_a
        .iter()
        .find(|k| keys_b.iter().any(|b| b == *k))
        .cloned()
}

impl Navigator {
    /// Build a navigator from an ordered-or-not policy chain, an executor, a budget, and config.
    pub fn new(
        policies: Vec<Box<dyn SearchPolicy>>,
        executor: Box<dyn SearchExecutor>,
        budget: Budget,
        config: NavConfig,
    ) -> Self {
        Self {
            policies,
            executor,
            budget,
            config,
        }
    }

    /// Escalation criterion (engine-arch §12.4): a tier above `Rules` may only choose when the
    /// cheaper tiers abstained AND there is a positive-value (`expected_gain > cost`),
    /// admissible, unseen action AND the budget permits it. An unresolved constraint is
    /// represented by such an action existing after cheaper tiers declined; callers may also
    /// set `ctx.uncovered` explicitly.
    fn escalation_ok(&self, state: &SearchState, actions: &[SearchAction], ctx: &NavContext) -> bool {
        !ctx.budget.exhausted()
            && actions.iter().any(|a| {
                a.admissible && !state.seen(&a.action) && a.expected_gain > a.cost
            })
    }

    /// Run the tier chain in order of cost and return the cheapest sufficient choice.
    fn choose(
        &self,
        state: &SearchState,
        actions: &[SearchAction],
        ctx: &NavContext,
    ) -> Option<(SearchAction, PolicyTier)> {
        let mut order: Vec<&Box<dyn SearchPolicy>> = self.policies.iter().collect();
        order.sort_by_key(|p| p.tier());
        for p in order {
            if p.tier() != PolicyTier::Rules && !self.escalation_ok(state, actions, ctx) {
                continue;
            }
            if let Some(a) = p.choose(state, actions) {
                if a.admissible && !state.seen(&a.action) && ctx.budget.can_afford(a.cost) {
                    return Some((a, p.tier()));
                }
            }
        }
        None
    }

    /// Best-first traversal over a bounded beam; logs each step with the tier actually used.
    pub fn run(&mut self, query: impl Into<String>) -> NavOutcome {
        let query = query.into();
        let mut ctx = NavContext::new(self.budget.clone());
        let mut trace: Vec<Step> = Vec::new();
        let beam = self.config.beam.max(1);

        let mut frontier: Vec<Node> = vec![Node::root(&query)];
        let mut best = frontier[0].clone();
        let mut last_len = best.state.results.len();
        let mut stall: u8 = 0;
        let stop;

        loop {
            if ctx.solved {
                stop = StopReason::Solved;
                break;
            }
            if ctx.past_deadline() {
                stop = StopReason::Deadline;
                break;
            }
            if ctx.budget.exhausted() {
                stop = StopReason::Budget;
                break;
            }
            if frontier.is_empty() {
                stop = StopReason::NoActions;
                break;
            }

            let mut children: Vec<Node> = Vec::new();

            for node in frontier.iter() {
                if node.state.hops >= self.config.max_hops {
                    continue;
                }
                let cands: Vec<SearchAction> = self
                    .executor
                    .candidates(&node.state, &[])
                    .into_iter()
                    .filter(|a| a.admissible && !node.state.seen(&a.action))
                    .collect();
                if cands.is_empty() {
                    continue;
                }

                let groups = group_by_key(&cands);
                let num_groups = groups.len();
                let base_cap = (beam + 1).saturating_sub(num_groups).max(1);
                let cap = base_cap.max(self.config.reserve_per_interpretation.max(1));

                // The cheapest sufficient tier picks the primary expansion.
                let mut chosen: Vec<(SearchAction, PolicyTier)> = Vec::new();
                let mut counts: HashMap<String, usize> = HashMap::new();
                if let Some((a, t)) = self.choose(&node.state, &cands, &ctx) {
                    *counts.entry(SearchState::key_of(&a.action)).or_insert(0) += 1;
                    chosen.push((a, t));
                }

                // Reserved slots for the other credible interpretations (beam diversity).
                for g in &groups {
                    if chosen.len() >= beam {
                        break;
                    }
                    let used = counts.get(&g.tag).copied().unwrap_or(0);
                    if used >= cap {
                        continue;
                    }
                    if chosen
                        .iter()
                        .any(|(a, _)| SearchState::key_of(&a.action) == g.tag)
                    {
                        continue;
                    }
                    let (act, tier) = match self.choose(&node.state, &g.members, &ctx) {
                        Some((a, t)) => (a, t),
                        None => (g.best.clone(), PolicyTier::Rules),
                    };
                    *counts.entry(g.tag.clone()).or_insert(0) += 1;
                    chosen.push((act, tier));
                }
                if chosen.is_empty() {
                    continue;
                }

                for (act, tier) in chosen {
                    if ctx.budget.exhausted() || !ctx.budget.can_afford(act.cost) {
                        break;
                    }
                    if node.state.seen(&act.action) {
                        continue;
                    }
                    let key = SearchState::key_of(&act.action);
                    ctx.budget.charge(act.cost);
                    let mut child_state = node.state.clone();
                    child_state.record(&act.action);
                    child_state.hops = child_state.hops.saturating_add(1);
                    ctx.executed.push(key.clone());
                    if let Err(e) = self.executor.execute(&act.action, &mut child_state, &mut ctx) {
                        ctx.notes.push(format!("execute failed for {key}: {e}"));
                    }
                    ctx.budget.used_ms = ctx.budget.used_ms.max(ctx.elapsed_ms);
                    trace.push(Step {
                        action: act.action.clone(),
                        reason: format!("{} [{}]", act.action.slug(), key),
                        used_policy: tier,
                    });
                    let score = node.score + act.expected_gain;
                    children.push(Node {
                        state: child_state,
                        score,
                        tag: key,
                    });
                }
                if ctx.budget.exhausted() {
                    break;
                }
            }

            if children.is_empty() {
                stop = StopReason::NoActions;
                break;
            }

            frontier = select_beam(children, beam, self.config.reserve_per_interpretation);

            if let Some(primary) = frontier.iter().max_by(|a, b| cmp_node(a, b)) {
                if better(primary, &best) {
                    best = primary.clone();
                }
            }
            if best.state.results.len() > last_len {
                last_len = best.state.results.len();
                stall = 0;
            } else if self.config.plateau_patience > 0 {
                stall = stall.saturating_add(1);
                if stall >= self.config.plateau_patience {
                    ctx.notes.push("plateau: no new documents".to_string());
                    stop = StopReason::Plateau;
                    break;
                }
            }

            if ctx.solved {
                stop = StopReason::Solved;
                break;
            }
            if ctx.past_deadline() {
                stop = StopReason::Deadline;
                break;
            }
            if ctx.budget.exhausted() {
                stop = StopReason::Budget;
                break;
            }
        }

        NavOutcome {
            state: best.state,
            stop,
            trace,
        }
    }
}
