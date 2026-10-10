//! Query planning: turn a natural-language question into search probes.
//!
//! Why this exists. Without a planner the engine treats a question as a bag of words: the rule
//! analyzer splits on whitespace and the lexical lane runs a plain disjunction, so *"how does flash
//! attention reduce memory"* matches whatever merely mentions "memory". Nothing resolves pronouns,
//! nothing carries the previous turn, and the entity graph is never seeded from what the question
//! actually names. The architecture (§14–§19) asks for an LLM to deconstruct the turn into canonical
//! entities, exact phrases with proximity, and negative constraints, and to hand those to the
//! retrieval lanes.
//!
//! The planner emits a **Tantivy query string**, which is what makes this cheap to adopt: the parser
//! already honours `"a b"~3` (phrase proximity), `+term` (required) and `-term` (excluded), so no new
//! query machinery is needed for a plan to take effect. The `entities` list is what the §9.4 graph
//! lane seeds its activation from.
//!
//! Caching: this goes through the shared [`LlmClient`], whose cache key is
//! `sha256(base_url, model, prompt)`. Repeating a question — or a near-identical one over an
//! unchanged turn — costs **no tokens**.
//!
//! Failure policy: an unparseable plan is `Ok(None)`, never an error and never a guess. The caller
//! keeps the user's own words, which is what the engine did before planning existed.

use crate::client::LlmClient;
use crate::error::OrganizeError;
use crate::prompt::extract_json;

/// What the planner decided, in the form the retrieval lanes consume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPlan {
    /// The lexical query: a Tantivy query string (`+required "phrase"~3 -excluded`), or empty when
    /// the planner had nothing better than the original question.
    pub query: String,
    /// Canonical entity names the question names. Seeds the §9.4 graph walk.
    pub entities: Vec<String>,
    /// Exact phrases worth matching closely, as they appear in `query`.
    pub phrases: Vec<String>,
    /// Terms the planner decided to exclude.
    pub negatives: Vec<String>,
}

impl QueryPlan {
    /// Whether this plan carries anything the caller should prefer over the raw question.
    pub fn is_usable(&self) -> bool {
        !self.query.trim().is_empty() || !self.entities.is_empty()
    }
}

/// The prompt. Kept explicit about the query syntax because that is the contract: the parser is
/// Tantivy's, and the planner's job is to speak it.
pub fn build_query_prompt(question: &str, history: &[String]) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are the query planner of a local search engine. Turn the user's question into probes.\n\
         \n\
         Rules:\n\
         - `query` is a Tantivy query string.\n\
         - `+term` means REQUIRED. Use it for the core nouns the answer must contain.\n\
         - `-term` means EXCLUDED. Use it for senses the user clearly does not want.\n\
         - `\"two words\"~3` means the words within 3 tokens: use it for exact phrases whose meaning\n\
         depends on adjacency (\"memory complexity\"), instead of two loose words.\n\
         - Bare words are optional matches. Prefer a few precise probes over many vague ones.\n\
         - `entities` are canonical proper-noun entities the question names (\"FlashAttention\",\n\
         \"KV Cache\", \"vLLM\"). Resolve pronouns and nicknames from the conversation; do not invent\n\
         entities that the question does not imply.\n\
         - If the question is a bare keyword search, return it unchanged as `query` with no entities.\n\
         - NEVER answer the question. You are planning retrieval, not answering.\n\
         \n\
         Return STRICT JSON ONLY. No prose, no markdown fences. Exactly this shape:\n\
         {\"query\": \"...\", \"entities\": [\"...\"], \"phrases\": [\"...\"], \"negatives\": [\"...\"]}\n",
    );
    if !history.is_empty() {
        prompt.push_str("\nConversation so far (oldest first), for resolving references:\n");
        for turn in history.iter().take(6) {
            prompt.push_str("- ");
            prompt.push_str(turn.trim());
            prompt.push('\n');
        }
    }
    prompt.push_str("\nQuestion: ");
    prompt.push_str(question.trim());
    prompt.push('\n');
    prompt
}

/// Parse the planner's reply. Forgiving about framing (fences, surrounding prose), strict about
/// shape: a missing or non-string field is `None` rather than a guessed default.
pub fn parse_plan(raw: &str) -> Option<QueryPlan> {
    let json = extract_json(raw)?;
    let value: serde_json::Value = serde_json::from_str(&json).ok()?;
    let object = value.as_object()?;

    let raw_query = object.get("query").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let list = |key: &str| -> Vec<String> {
        object
            .get(key)
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(|item| item.trim().to_string())
                    .filter(|item| !item.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    };

    let phrases = list("phrases");
    // Fold the phrases into the query. Retrieval only ever sees one string, and a live run showed
    // the model reporting phrases separately (`phrases: ["memory complexity"]` with a query of
    // `+flash +attention +memory`), so a phrase it identified would otherwise be silently lost —
    // which is exactly the bug this planner exists to fix. `"a b"~3` is the syntax that makes
    // adjacency matter instead of two loose words.
    let mut query = raw_query;
    for phrase in &phrases {
        if phrase.split_whitespace().count() > 1 && !query.contains(phrase.as_str()) {
            query.push_str(&format!(" \"{phrase}\"~3"));
        }
    }
    let query = query.trim().to_string();

    let plan = QueryPlan {
        query,
        entities: list("entities"),
        phrases,
        negatives: list("negatives"),
    };
    // An empty plan is not a plan; the caller keeps the user's words.
    if plan.is_usable() {
        Some(plan)
    } else {
        None
    }
}

impl LlmClient {
    /// Plan `question` (with optional conversation history). `Ok(None)` when the model's reply could
    /// not be parsed — the caller falls back to the raw question rather than failing the search.
    pub async fn plan_query(
        &self,
        question: &str,
        history: &[String],
    ) -> Result<Option<QueryPlan>, OrganizeError> {
        let prompt = build_query_prompt(question, history);
        let raw = self.complete(&prompt).await?;
        Ok(parse_plan(&raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_plan_with_every_field() {
        let raw = r#"{"query": "+\"flash attention\"~3 +memory -winter", "entities": ["FlashAttention", "KV Cache"], "phrases": ["flash attention"], "negatives": ["winter"]}"#;
        let plan = parse_plan(raw).expect("a well-formed plan must parse");
        assert_eq!(plan.query, "+\"flash attention\"~3 +memory -winter");
        assert_eq!(plan.entities, vec!["FlashAttention", "KV Cache"]);
        assert_eq!(plan.phrases, vec!["flash attention"]);
        assert_eq!(plan.negatives, vec!["winter"]);
        assert!(plan.is_usable());
    }

    #[test]
    fn phrases_are_folded_into_the_query_the_parser_actually_sees() {
        // The model reports phrases separately; retrieval only sees `query`. A phrase it identified
        // must not be lost between the two.
        let raw = r#"{"query": "+flash +attention +memory", "entities": ["FlashAttention"], "phrases": ["memory complexity"]}"#;
        let plan = parse_plan(raw).expect("plan");
        assert!(
            plan.query.contains("\"memory complexity\"~3"),
            "the phrase must be folded in with proximity: {}",
            plan.query
        );
        assert!(!plan.query.contains("\"memory\"~3"), "single words are not phrases");
    }

    #[test]
    fn a_phrase_already_in_the_query_is_not_duplicated() {
        let raw = r#"{"query": "+\"kv cache\"~2 +offload", "phrases": ["kv cache"]}"#;
        let plan = parse_plan(raw).expect("plan");
        assert_eq!(plan.query.matches("kv cache").count(), 1, "no duplication: {}", plan.query);
    }

    #[test]
    fn tolerates_fences_and_surrounding_prose() {
        let raw = "Sure, here is the plan:\n```json\n{\"query\": \"+zeta\", \"entities\": []}\n```\n";
        let plan = parse_plan(raw).expect("framing must not break parsing");
        assert_eq!(plan.query, "+zeta");
        assert!(plan.entities.is_empty());
    }

    #[test]
    fn refuses_a_plan_that_says_nothing() {
        // An empty plan must be None, not an empty query that would silently replace the user's own
        // words with nothing.
        assert!(parse_plan(r#"{"query": "", "entities": []}"#).is_none());
        assert!(parse_plan("not json at all").is_none());
        assert!(parse_plan("{}").is_none());
        assert!(parse_plan("").is_none());
    }

    #[test]
    fn entities_alone_are_a_usable_plan() {
        // A question that is only an entity ("vLLM?") still seeds the graph lane even with no query
        // rewrite.
        let plan = parse_plan(r#"{"query": "", "entities": ["vLLM"]}"#).expect("entity-only plan");
        assert!(plan.is_usable());
        assert_eq!(plan.entities, vec!["vLLM"]);
    }

    #[test]
    fn the_prompt_teaches_the_query_syntax_because_that_is_the_contract() {
        let prompt = build_query_prompt("how does flash attention reduce memory", &[]);
        assert!(prompt.contains("~3"), "proximity syntax must be explained");
        assert!(prompt.contains("+term"), "required terms must be explained");
        assert!(prompt.contains("-term"), "exclusions must be explained");
        assert!(prompt.contains("STRICT JSON"));
        assert!(prompt.contains("how does flash attention reduce memory"));
        // Never answer: the planner plans retrieval.
        assert!(prompt.contains("NEVER answer the question"));
    }

    #[test]
    fn history_is_included_so_pronouns_can_be_resolved() {
        let prompt = build_query_prompt(
            "how does it reduce memory?",
            &["what is flash attention".to_string()],
        );
        assert!(prompt.contains("what is flash attention"));
        assert!(prompt.contains("how does it reduce memory?"));
    }
}
