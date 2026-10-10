//! Prompt construction and plan parsing.
//!
//! The prompt is deliberately small and bounded, and the response is parsed defensively: a model
//! that returns prose, a fenced block, an over-long probe, or a probe that merely repeats the
//! query is handled by discarding the bad part, never by failing the search. The engine applies
//! `variant_preserves_constraints` and the variant cap on top, so a probe that drops a constraint
//! is rejected there even if it survives parsing here.

use search_engine::analyze::QueryAnalysis;
use search_engine::PlannedQueries;

/// Bumped whenever the prompt text or the caps change; part of the cache key (via the prompt
/// bytes) so a new prompt version never reuses a plan produced under older instructions.
pub const PROMPT_VERSION: &str = "plan-v1";

/// Hard cap on the assembled prompt, in characters. **Never** exceed this on the wire.
pub const MAX_PROMPT_CHARS: usize = 4000;
/// Cap on any single expansion probe's length.
pub const MAX_EXPANSION_CHARS: usize = 200;
/// Cap on analysis hint terms embedded in the prompt.
const MAX_HINT_TERMS: usize = 12;
/// Cap on the free-text note kept from the model's response.
const MAX_NOTE_CHARS: usize = 300;

/// Stable instruction header. Kept verbatim so a test can pin the prompt shape.
const HEADER: &str = "\
You are the query planner of a search engine. Deconstruct the user's query into a small number of \
search probes that would find the same information written differently.
Reply with ONLY a JSON object, no prose and no code fence:
{\"expansions\": [\"<probe>\", ...], \"notes\": \"<short explanation>\"}
Rules:
- 1 to 3 probes. Each probe is a Tantivy query string.
- For a multi-word concept, use a quoted phrase with proximity slop, e.g. \"memory complexity\"~3.
- If the query uses a pronoun or a nickname, replace it with the canonical name of the entity.
- Never add a constraint the query did not have, and never drop one it did (identifiers, quoted
  phrases, negations, dates). Probes that drop a constraint are discarded.
- Do not repeat the original query verbatim.
- Prefer 2 to 4 content words per probe; do not write a sentence.
";

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Build the planning prompt from the query and the deterministic analysis already computed for
/// it. The analysis is offered as *hints* (the phrases, identifiers and content terms the rule
/// analyzer found) so the model can preserve the query's hard constraints.
///
/// The whole prompt is truncated to [`MAX_PROMPT_CHARS`].
pub fn build_prompt(query: &str, analysis: &QueryAnalysis) -> String {
    let mut p = String::with_capacity(512);
    p.push_str(HEADER);
    p.push_str("\nQuery:\n");
    p.push_str(query.trim());
    p.push_str("\n\nHints from the lexical analyzer:\n");
    if !analysis.identifiers.is_empty() {
        p.push_str(&format!(
            "- identifiers (must survive): {}\n",
            analysis.identifiers.iter().take(MAX_HINT_TERMS).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if !analysis.phrases.is_empty() {
        p.push_str(&format!(
            "- quoted phrases (must survive): {}\n",
            analysis.phrases.iter().take(MAX_HINT_TERMS).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if !analysis.terms.is_empty() {
        p.push_str(&format!(
            "- content terms: {}\n",
            analysis.terms.iter().take(MAX_HINT_TERMS).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    if analysis.identifiers.is_empty() && analysis.phrases.is_empty() && analysis.terms.is_empty() {
        p.push_str("- (none)\n");
    }
    truncate_chars(&p, MAX_PROMPT_CHARS)
}

/// A parse failure, reported as a note rather than an error the search would have to handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanParseError {
    /// No JSON object could be located in the response.
    NoJson,
    /// The JSON was located but is not a usable plan object.
    Shape(String),
}

impl std::fmt::Display for PlanParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanParseError::NoJson => write!(f, "no JSON object in the model response"),
            PlanParseError::Shape(s) => write!(f, "unusable plan object: {s}"),
        }
    }
}

/// Extract the outermost JSON object from a response that may carry prose or a code fence around
/// it. Returns the substring from the first `{` to the matching final `}`.
fn extract_json_object(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let end = s.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(&s[start..=end])
}

/// Parse a model response into [`PlannedQueries`].
///
/// * `expansions` must be an array of strings; empty, whitespace-only, over-long, or
///   original-echoing entries are dropped, duplicates are removed, and the list is capped at
///   `max_expansions` (the engine would cap it again).
/// * `notes` accepts either a string or an array of strings.
/// * A response with no usable JSON, or with an empty `expansions`, yields the identity plan
///   (empty expansions) — never an error the caller must handle.
pub fn parse_plan(content: &str, max_expansions: usize, original: &str) -> Result<PlannedQueries, PlanParseError> {
    let json = extract_json_object(content).ok_or(PlanParseError::NoJson)?;
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| PlanParseError::Shape(e.to_string()))?;

    let mut expansions: Vec<String> = Vec::new();
    let original_trimmed = original.trim();
    if let Some(arr) = value.get("expansions").and_then(|v| v.as_array()) {
        for item in arr {
            let Some(s) = item.as_str() else { continue };
            let t = s.trim();
            if t.is_empty() || t == original_trimmed {
                continue;
            }
            let t = truncate_chars(t, MAX_EXPANSION_CHARS);
            if !expansions.contains(&t) {
                expansions.push(t);
            }
            if expansions.len() >= max_expansions {
                break;
            }
        }
    }

    let mut notes: Vec<String> = Vec::new();
    match value.get("notes") {
        Some(serde_json::Value::String(s)) => {
            let t = s.trim();
            if !t.is_empty() {
                notes.push(truncate_chars(t, MAX_NOTE_CHARS));
            }
        }
        Some(serde_json::Value::Array(arr)) => {
            for item in arr {
                if let Some(s) = item.as_str() {
                    let t = s.trim();
                    if !t.is_empty() {
                        notes.push(truncate_chars(t, MAX_NOTE_CHARS));
                    }
                }
            }
        }
        _ => {}
    }

    Ok(PlannedQueries { expansions, notes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use search_engine::analyze::analyze_query;

    fn analysis(q: &str) -> QueryAnalysis {
        analyze_query(q, 0, 0)
    }

    #[test]
    fn prompt_pins_the_json_contract_and_the_hints() {
        let a = analysis("what is the memory complexity of FlashAttention v2");
        let p = build_prompt("what is the memory complexity of FlashAttention v2", &a);
        assert!(p.contains("Reply with ONLY a JSON object"));
        assert!(p.contains("\"expansions\""));
        assert!(p.contains("proximity slop"));
        assert!(p.contains("Query:"));
        assert!(p.contains("Hints from the lexical analyzer:"));
    }

    #[test]
    fn prompt_is_bounded() {
        let q = "term ".repeat(5000);
        let a = analysis(&q);
        let p = build_prompt(&q, &a);
        assert!(p.chars().count() <= MAX_PROMPT_CHARS);
    }

    #[test]
    fn parses_a_clean_plan() {
        let body = r#"{"expansions": ["flash attention memory", "\"memory complexity\"~3"], "notes": "entities + phrase"}"#;
        let p = parse_plan(body, 3, "memory complexity of flash attention").unwrap();
        assert_eq!(p.expansions.len(), 2);
        assert_eq!(p.expansions[1], "\"memory complexity\"~3");
        assert_eq!(p.notes, vec!["entities + phrase".to_string()]);
    }

    #[test]
    fn parses_json_wrapped_in_prose_or_a_fence() {
        let body = "Sure! Here is the plan:\n```json\n{\"expansions\":[\"paged attention\"]}\n```\n";
        let p = parse_plan(body, 3, "q").unwrap();
        assert_eq!(p.expansions, vec!["paged attention".to_string()]);
    }

    #[test]
    fn drops_the_original_duplicates_blanks_and_caps() {
        let body = r#"{"expansions": ["q", "", "  ", "a", "a", "b", "c", "d"]}"#;
        let p = parse_plan(body, 3, "q").unwrap();
        assert_eq!(p.expansions, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    #[test]
    fn truncates_an_over_long_probe() {
        let long = "x".repeat(1000);
        let body = format!(r#"{{"expansions": ["{long}"]}}"#);
        let p = parse_plan(&body, 3, "q").unwrap();
        assert_eq!(p.expansions[0].chars().count(), MAX_EXPANSION_CHARS);
    }

    #[test]
    fn no_json_is_an_error_not_a_panic() {
        assert_eq!(parse_plan("I cannot help with that.", 3, "q"), Err(PlanParseError::NoJson));
    }

    #[test]
    fn missing_expansions_yields_the_identity_plan() {
        let p = parse_plan(r#"{"notes":"nothing to add"}"#, 3, "q").unwrap();
        assert!(p.expansions.is_empty());
        assert!(p.is_empty());
    }
}
