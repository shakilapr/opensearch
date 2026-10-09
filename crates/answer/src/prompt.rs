//! Prompt construction and response parsing for answer generation.
//!
//! The prompt is deliberately small and bounded: the caller caps the number of passages, the
//! characters per passage, the number of evidence records, and the total prompt size. The model
//! is instructed to answer **only** from the supplied evidence, to say so plainly when the
//! evidence is insufficient, and to cite the passage indices it used.

use crate::error::AnswerError;
use search_engine::evidence::EvidenceRecord;

/// Bumped whenever the prompt text or the caps change; part of the cache key so a new prompt
/// version never reuses an answer produced under older instructions.
pub const PROMPT_VERSION: &str = "answer-v1";

/// Cap on the number of passages embedded in one prompt.
pub const MAX_PASSAGES: usize = 8;
/// Cap on the characters taken from any single passage (truncated, not dropped).
pub const MAX_CHARS_PER_PASSAGE: usize = 1000;
/// Cap on the number of evidence records embedded in one prompt.
pub const MAX_EVIDENCE_RECORDS: usize = 24;
/// Hard cap on the whole assembled prompt, in characters. **Never** exceed this on the wire.
pub const MAX_PROMPT_CHARS: usize = 8000;

/// Stable header lines. Kept verbatim so tests can pin the prompt shape with a golden substring.
const HEADER: &str = "\
Answer the user's query using ONLY the evidence and passages listed below.
Rules:
- Use only the supplied evidence; do not use outside knowledge.
- If the evidence is insufficient to answer, say so plainly.
- Cite the passage indices you used, e.g. [0][2].
- Be concise.
";

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Build the single answer prompt from the query, evidence records and passages.
///
/// Bounds (all in this module):
/// * at most [`MAX_EVIDENCE_RECORDS`] evidence lines;
/// * at most [`MAX_PASSAGES`] passages, each truncated to [`MAX_CHARS_PER_PASSAGE`];
/// * the whole prompt truncated to [`MAX_PROMPT_CHARS`].
pub fn build_prompt(query: &str, evidence: &[EvidenceRecord], passages: &[String]) -> String {
    let mut p = String::with_capacity(512);
    p.push_str(HEADER);
    p.push_str("\nQuery:\n");
    p.push_str(query.trim());
    p.push_str("\n\nEvidence:\n");
    if evidence.is_empty() {
        p.push_str("(none)\n");
    } else {
        for (i, e) in evidence.iter().take(MAX_EVIDENCE_RECORDS).enumerate() {
            p.push_str(&format!(
                "- [{i}] unit={} doc={} status={:?} version={}\n",
                e.unit_id, e.doc_id, e.status, e.doc_version
            ));
        }
    }
    p.push_str("\nPassages:\n");
    if passages.is_empty() {
        p.push_str("(none)\n");
    } else {
        for (i, s) in passages.iter().take(MAX_PASSAGES).enumerate() {
            let t = truncate_chars(s.trim(), MAX_CHARS_PER_PASSAGE);
            p.push_str(&format!("[{i}] {t}\n"));
        }
    }
    // Hard budget on the whole request payload.
    truncate_chars(&p, MAX_PROMPT_CHARS)
}

/// Parse an OpenAI-chat-compatible response **body** into the assistant answer.
///
/// * non-empty `content` -> `Ok`;
/// * `finish_reason == "length"` (with null/empty content) -> [`AnswerError::Truncated`], whose
///   message names `SEARCH_ANSWER_MAX_TOKENS`;
/// * anything else with no usable content -> [`AnswerError::EmptyContent`].
pub fn parse_chat_response(body: &str) -> Result<String, AnswerError> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| AnswerError::Http(format!("invalid JSON response: {e}")))?;
    let choice = value.get("choices").and_then(|c| c.get(0));
    let content = choice
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str());

    match content {
        Some(s) if !s.trim().is_empty() => Ok(s.to_string()),
        _ => {
            let finish = choice
                .and_then(|c| c.get("finish_reason"))
                .and_then(|f| f.as_str())
                .unwrap_or("?");
            if finish == "length" {
                Err(AnswerError::Truncated(finish.to_string()))
            } else {
                Err(AnswerError::EmptyContent)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(unit: &str, doc: &str) -> EvidenceRecord {
        EvidenceRecord {
            unit_id: unit.to_string(),
            doc_id: doc.to_string(),
            status: search_engine::evidence::Support::Supported,
            span: None,
            doc_version: "v1".to_string(),
        }
    }

    #[test]
    fn prompt_contains_the_stable_instruction_header() {
        let p = build_prompt("rust async", &[rec("u1", "d1")], &["a passage".to_string()]);
        assert!(p.contains("using ONLY the evidence and passages listed below"));
        assert!(p.contains("If the evidence is insufficient to answer, say so plainly"));
        assert!(p.contains("Cite the passage indices you used"));
        assert!(p.contains("[0] a passage"));
    }

    #[test]
    fn prompt_is_bounded() {
        let passages: Vec<String> = (0..1000).map(|i| format!("passage {i} ").repeat(200)).collect();
        let p = build_prompt("q", &[], &passages);
        assert!(p.chars().count() <= MAX_PROMPT_CHARS);
    }

    #[test]
    fn length_finish_reason_mentions_the_budget() {
        let body = r#"{"choices":[{"finish_reason":"length","message":{"content":null}}]}"#;
        let err = parse_chat_response(body).unwrap_err().to_string();
        assert!(err.contains("SEARCH_ANSWER_MAX_TOKENS"), "got: {err}");
    }

    #[test]
    fn empty_content_is_an_error() {
        let body = r#"{"choices":[{"finish_reason":"stop","message":{"content":"   "}}]}"#;
        assert!(matches!(
            parse_chat_response(body),
            Err(AnswerError::EmptyContent)
        ));
    }
}
