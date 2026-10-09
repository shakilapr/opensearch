//! Query-text normalization shared by retrieval and evidence verification.
//!
//! The index tokenizer is a plain lowercase tokenizer with no stop-word filter
//! (changing the tokenizer would require re-indexing), so the *query* side has
//! to carry the stop-word knowledge. Without it a natural-language query such as
//! "what is a transformer" is parsed as an OR over `what`, `is`, `a` and
//! `transformer`; the single-letter token `a` matches nearly every document and
//! the ranking becomes noise — a live run returned cookbook budget pages and
//! chemistry pages for that query.
//!
//! [`prune_query`] strips those terms while preserving the query syntax the
//! parser understands: `field:"value"` clauses, required (`+`) and excluded
//! (`-`) prefixes, and quoted phrases.
//!
//! [`content_tokens`] gives the same notion of "content" to callers that need
//! tokens rather than a query string (e.g. coverage verification).

/// English function words that carry no retrieval signal. Kept deliberately
/// small and conventional: every entry must be safe to drop from a query.
pub const STOPWORDS: &[&str] = &[
    "a", "about", "an", "and", "are", "as", "at", "be", "been", "but", "by", "can", "could", "did",
    "do", "does", "for", "from", "had", "has", "have", "how", "in", "into", "is", "it", "its",
    "may", "might", "of", "on", "or", "our", "shall", "should", "so", "than", "that", "the",
    "their", "them", "then", "there", "these", "they", "this", "to", "was", "we", "were", "what",
    "when", "where", "which", "who", "why", "will", "with", "would", "you", "your",
];

/// True when `term` (already lower-cased) is a function word or a single
/// character — neither can discriminate between documents.
pub fn is_stopword(term: &str) -> bool {
    term.chars().count() < 2 || STOPWORDS.contains(&term)
}

/// Content words of `text`: lowercase alphanumeric runs that are neither
/// stop-words nor single characters.
pub fn content_tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty() && !is_stopword(t))
        .map(|t| t.to_string())
        .collect()
}

/// Split a query into whitespace-separated tokens, keeping quoted spans (and any
/// `field:` prefix) together so `+doc_type:"pdf"` survives as one token.
fn split_tokens(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in query.chars() {
        if c == '"' {
            quoted = !quoted;
            cur.push(c);
            continue;
        }
        if c.is_whitespace() && !quoted {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            continue;
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// True when a token is query syntax rather than free text and must be passed
/// through untouched: quoted, field-qualified, or required/excluded.
fn is_syntax(token: &str) -> bool {
    token.starts_with('"')
        || token.ends_with('"')
        || token.contains(':')
        || token.starts_with('+')
        || token.starts_with('-')
        || token.starts_with('(')
        || token.ends_with(')')
}

/// Drop stop-words and single-character terms from a free-text query.
///
/// Falls back to the original query when pruning would leave nothing (a query
/// that is nothing but function words must still search for something). Query
/// syntax — phrases, field clauses, `+`/`-` operators — is preserved verbatim,
/// so a caller can safely append facets before or after pruning.
pub fn prune_query(query: &str) -> String {
    let mut kept: Vec<String> = Vec::new();
    for token in split_tokens(query) {
        if is_syntax(&token) {
            kept.push(token);
            continue;
        }
        // A token may be punctuation-joined ("state-of-the-art"): keep the parts
        // that carry content, drop the rest.
        let parts: Vec<String> = content_tokens(&token);
        if !parts.is_empty() {
            kept.push(parts.join(" "));
        }
    }
    if kept.is_empty() {
        return query.trim().to_string();
    }
    let pruned = kept.join(" ");
    if pruned.trim().is_empty() {
        query.trim().to_string()
    } else {
        pruned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_drops_function_words_and_single_letters() {
        assert_eq!(prune_query("what is a transformer"), "transformer");
        assert_eq!(prune_query("How does vLLM paged attention work"), "vllm paged attention work");
        assert_eq!(prune_query("retrieval augmented generation"), "retrieval augmented generation");
    }

    #[test]
    fn prune_preserves_query_syntax() {
        // Facets and operators are not free text: they survive verbatim.
        assert_eq!(
            prune_query(r#"rust async +source_id:"news" +doc_type:"pdf""#),
            r#"rust async +source_id:"news" +doc_type:"pdf""#
        );
        // A quoted phrase stays together.
        assert_eq!(prune_query(r#""what is a transformer""#), r#""what is a transformer""#);
        // Hyphenated compounds are split into their content parts.
        assert_eq!(prune_query("state-of-the-art models"), "state art models");
    }

    #[test]
    fn prune_falls_back_when_nothing_remains() {
        // Nothing but function words: searching for nothing returns nothing, so
        // the original query is kept instead.
        assert_eq!(prune_query("what is it"), "what is it");
        assert_eq!(prune_query("   "), "");
    }

    #[test]
    fn content_tokens_are_lowercased_content_words() {
        assert_eq!(content_tokens("How does Paged-Attention work?"), vec!["paged", "attention", "work"]);
        assert!(content_tokens("a is the of").is_empty());
    }
}
