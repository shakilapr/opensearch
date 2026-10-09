//! Entity mention extraction and co-occurrence edges over document text (plan Stage 1:
//! "entity aliases + concept graph").
//!
//! This is deliberately **lexical and deterministic** — no model, no network, no clock — so a
//! graph build is reproducible and cheap enough to run over a corpus of ten thousand pages.
//! It is not a state-of-the-art NER system and does not pretend to be: the point is a populated
//! `entity` / `entity_alias` / `graph_edge` store whose *shape* (canonical id, aliases,
//! typed weighted edges) is what later retrieval and expansion need. A model-based extractor
//! can replace [`extract_mentions`] behind the same signature.
//!
//! ## Rules (documented because they are heuristics)
//!
//! 1. A **candidate** is a run of at most [`MAX_PHRASE_WORDS`] consecutive tokens that are all
//!    "mention-like": they begin with a capital, or are an acronym of two or more capitals, or
//!    mix case after the first character (`vLLM`, `iOS`), or contain a digit (`GPT-4`, `S3`).
//! 2. A candidate is **kept** only when its canonical form occurs at least `min_count` times in
//!    the document (case-insensitively). This is what removes sentence-initial noise: "However"
//!    and "This" start sentences once, while a real entity recurs.
//! 3. Leading and trailing function words are trimmed from a candidate ("The Rust Language"
//!    → "Rust Language"), and candidates that reduce to nothing are dropped.
//! 4. Adjacent identical candidates are one mention; the first occurrence wins for ordering.
//!
//! ## Kinds
//!
//! [`EntityKind`] is assigned from surface form only: an organisation suffix or academic word
//! makes it `Organization`, anything else is `Concept`. `Person` and `Place` are deliberately
//! never guessed — a wrong kind is worse than an unhelpful one, and the tag is a lookup hint,
//! not a claim.

use std::collections::HashMap;

use crate::EntityKind;

/// Longest phrase, in words, that may become one mention.
pub const MAX_PHRASE_WORDS: usize = 4;

/// Window, in tokens, inside which two mentions are considered related.
pub const DEFAULT_COOC_WINDOW: usize = 50;

/// Function words that may not begin or end a mention.
const BOUNDARY_STOPWORDS: &[&str] = &[
    "a", "an", "the", "this", "that", "these", "those", "it", "its", "in", "on", "of", "for",
    "with", "and", "or", "but", "if", "when", "while", "as", "at", "by", "from", "to", "is",
    "are", "was", "were", "be", "been", "being", "we", "you", "they", "he", "she", "his", "her",
    "their", "our", "your", "my", "not", "no", "so", "such", "than", "then", "there", "here",
    "however", "therefore", "also", "which", "who", "whom", "whose", "what", "all", "any",
    "both", "each", "few", "more", "most", "other", "some", "only", "own", "same", "too",
    "very", "can", "will", "just", "should", "now", "see", "using", "use", "used", "one", "two",
];

/// Words that, when they appear inside or after a candidate, mark it as an organisation.
const ORG_WORDS: &[&str] = &[
    "inc", "inc.", "corp", "corp.", "corporation", "ltd", "ltd.", "llc", "gmbh", "co",
    "company", "university", "universität", "institute", "institution", "lab", "labs",
    "laboratory", "foundation", "foundation.", "group", "team", "division", "department",
    "committee", "council", "association", "society", "agency", "bureau", "ministry", "bank",
    "linux", "mozilla", "google", "microsoft", "meta", "openai", "anthropic", "nvidia", "intel",
    "amd", "apple", "amazon", "ibm", "oracle", "cloudflare", "hugging", "face", "wikimedia",
];

/// One extracted surface form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    /// The surface form as it first appeared, trimmed.
    pub name: String,
    /// Canonical key: lowercase, punctuation collapsed to single spaces.
    pub key: String,
    /// Every distinct surface form seen for this key in the document, `name` first. This is what
    /// makes aliases possible: `vLLM` and `VLLM` are one key with two forms, so a resolver can
    /// match either without the extractor inventing a second entity.
    pub forms: Vec<String>,
    /// How many times the canonical form occurred in the document.
    pub count: usize,
    /// Token offset of the first occurrence, and the offset of the last token of that
    /// occurrence (both inclusive), so co-occurrence can be measured on real positions.
    pub first_token: usize,
    pub last_token: usize,
}

/// A canonical entity id for a surface form.
///
/// Same convention as `ResourceId::from_canonical_url` in `search-core`: the id is a stable
/// prefix plus the first 16 bytes of the SHA-256 of the canonical key, so the same entity is
/// the same id on every machine and every run, and two spellings that canonicalise together
/// share one node.
pub fn entity_id(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(key.as_bytes());
    let digest = h.finalize();
    format!("ent_{}", hex::encode(&digest[..16]))
}

/// Canonical key for a surface form: lowercase, non-alphanumeric runs collapsed to one space,
/// and surrounding space trimmed. `vLLM` and `VLLM` therefore share a node; `GPT-4` and
/// `GPT 4` do too.
pub fn canonical_key(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut pending_space = false;
    for ch in name.chars() {
        if ch.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
        } else {
            pending_space = true;
        }
    }
    out
}

/// The kind implied by a surface form (rules in the module docs).
pub fn kind_for(name: &str) -> EntityKind {
    let lower = name.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    if words.iter().any(|w| ORG_WORDS.contains(w)) {
        EntityKind::Organization
    } else {
        EntityKind::Concept
    }
}

/// A token with its position, so mentions can be compared by distance.
#[derive(Debug, Clone)]
struct Token {
    text: String,
    is_word: bool,
}

/// Tokenise into words and separator runs, so that adjacency of two tokens means they were
/// adjacent in the source. Separator tokens are kept (not discarded) precisely so a mention run
/// cannot jump across punctuation: dropping them turned `NumPy. NumPy` into one mention.
fn tokenize(text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens: Vec<Token> = Vec::new();
    let mut word = String::new();
    let mut sep = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        let continues_word = !word.is_empty()
            && matches!(ch, '+' | '#' | '-' | '.' | '_')
            && chars.get(i + 1).is_some_and(|c| c.is_alphanumeric());
        if ch.is_alphanumeric() || continues_word {
            if !sep.is_empty() {
                tokens.push(Token { text: std::mem::take(&mut sep), is_word: false });
            }
            word.push(ch);
        } else {
            if !word.is_empty() {
                tokens.push(Token { text: std::mem::take(&mut word), is_word: true });
            }
            sep.push(ch);
        }
        i += 1;
    }
    if !word.is_empty() {
        if !sep.is_empty() {
            tokens.push(Token { text: std::mem::take(&mut sep), is_word: false });
        }
        tokens.push(Token { text: word, is_word: true });
    } else if !sep.is_empty() {
        tokens.push(Token { text: sep, is_word: false });
    }
    tokens
}

/// Is this token shaped like the start (or continuation) of a mention?
fn is_mentionish(token: &str) -> bool {
    let mut chars = token.chars();
    let Some(first) = chars.next() else { return false };
    if !first.is_alphanumeric() {
        return false;
    }
    let uppercase = token.chars().filter(|c| c.is_uppercase()).count();
    let lowercase_after_first = token.chars().skip(1).any(|c| c.is_lowercase());
    first.is_uppercase()                    // Rust, Kubernetes, Anthropic
        || uppercase >= 2                   // vLLM, iOS, API, GPT
        || (lowercase_after_first && token.chars().any(|c| c.is_ascii_digit())) // b2b? GPT-4
}

fn is_stopword(text: &str) -> bool {
    BOUNDARY_STOPWORDS.contains(&text.to_lowercase().as_str())
}

/// Extract the mentions of a document body.
///
/// `min_count` is the recurrence threshold (see rule 2 in the module docs); `limit` caps the
/// number of mentions returned, keeping the most frequent first and breaking ties by first
/// occurrence so the result is deterministic.
pub fn extract_mentions(text: &str, min_count: usize, limit: usize) -> Vec<Mention> {
    let tokens = tokenize(text);
    if tokens.is_empty() {
        return Vec::new();
    }
    let min_count = min_count.max(1);

    // Word tokens, each with whether only whitespace separates it from the previous word: a
    // phrase may join across a space but never across punctuation or a sentence boundary.
    let words: Vec<(usize, &Token, bool)> = tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| t.is_word)
        .map(|(i, t)| {
            let space_before = match tokens[..i].iter().rev().find(|p| !p.is_word) {
                Some(sep) => sep.text.chars().all(char::is_whitespace),
                None => true,
            };
            (i, t, space_before)
        })
        .collect();
    if words.is_empty() {
        return Vec::new();
    }

    // Candidate spans over word tokens: maximal runs of mention-like words, capped in length,
    // with boundary function words trimmed.
    let mut spans: Vec<(usize, usize)> = Vec::new(); // indices into `words`
    let mut k = 0usize;
    while k < words.len() {
        if !is_mentionish(&words[k].1.text) {
            k += 1;
            continue;
        }
        let mut end = k;
        while end + 1 < words.len()
            && words[end + 1].2
            && is_mentionish(&words[end + 1].1.text)
            && end + 1 - k + 1 <= MAX_PHRASE_WORDS
        {
            end += 1;
        }
        let mut start = k;
        while start <= end && is_stopword(&words[start].1.text) {
            start += 1;
        }
        let mut last = end;
        while last >= start + 1 && is_stopword(&words[last].1.text) {
            last -= 1;
        }
        if start <= last {
            spans.push((start, last));
            k = end + 1;
        } else {
            k += 1;
        }
    }

    // Count canonical occurrences over the whole document (rule 2): a kept mention is one whose
    // key appears at least `min_count` times, which is what removes sentence-initial noise.
    let doc_lower: Vec<String> = words.iter().map(|(_, t, _)| canonical_key(&t.text)).collect();

    let mut by_key: HashMap<String, Mention> = HashMap::new();
    for (start, end) in spans {
        let name = words[start..=end]
            .iter()
            .map(|(_, t, _)| t.text.as_str())
            .collect::<Vec<&str>>()
            .join(" ");
        let key = canonical_key(&name);
        let key_words: Vec<&str> = key.split_whitespace().collect();
        if key_words.is_empty() {
            continue;
        }
        // Count occurrences as *adjacent word windows*, not as a substring search: a substring
        // search over " numpy numpy " is non-overlapping and would count the pair once, and it
        // would also match inside a longer token.
        let count = doc_lower
            .windows(key_words.len())
            .filter(|w| w.iter().map(String::as_str).eq(key_words.iter().copied()))
            .count();
        if count < min_count {
            continue;
        }
        let first_token = words[start].0;
        let last_token = words[end].0;
        match by_key.get_mut(&key) {
            Some(existing) => {
                existing.count = existing.count.max(count);
                if !existing.forms.contains(&name) {
                    existing.forms.push(name);
                }
            }
            None => {
                by_key.insert(
                    key.clone(),
                    Mention {
                        name: name.clone(),
                        key,
                        forms: vec![name],
                        count,
                        first_token,
                        last_token,
                    },
                );
            }
        }
    }

    let mut mentions: Vec<Mention> = by_key.into_values().collect();
    mentions.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.first_token.cmp(&b.first_token))
            .then_with(|| a.key.cmp(&b.key))
    });
    mentions.truncate(limit);
    mentions
}

/// Pairs of mentions that co-occur within `window` tokens, with their co-occurrence count.
///
/// Distance is measured between the *first* occurrence of each mention in the document, which
/// is why [`Mention`] carries token offsets: it makes the result deterministic for a given body
/// and independent of how many times each name recurs.
pub fn cooccurrence_pairs(mentions: &[Mention], window: usize) -> Vec<((String, String), f64)> {
    let mut pairs: HashMap<(String, String), f64> = HashMap::new();
    for (i, a) in mentions.iter().enumerate() {
        for b in mentions.iter().skip(i + 1) {
            let distance = b.first_token.abs_diff(a.first_token);
            if distance <= window {
                let (left, right) = if a.key <= b.key {
                    (a.key.clone(), b.key.clone())
                } else {
                    (b.key.clone(), a.key.clone())
                };
                *pairs.entry((left, right)).or_insert(0.0) += 1.0;
            }
        }
    }
    let mut out: Vec<((String, String), f64)> = pairs.into_iter().collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_key_folds_case_and_punctuation() {
        assert_eq!(canonical_key("Retrieval-Augmented Generation"), "retrieval augmented generation");
        assert_eq!(canonical_key("vLLM"), "vllm");
        assert_eq!(canonical_key("GPT-4"), "gpt 4");
        assert_eq!(canonical_key("  OpenAI "), "openai");
        assert_eq!(canonical_key("!!!"), "");
    }

    #[test]
    fn entity_id_is_stable_and_key_sensitive() {
        assert_eq!(entity_id("vllm"), entity_id("vllm"));
        assert_ne!(entity_id("vllm"), entity_id("transformers"));
        assert!(entity_id("vllm").starts_with("ent_"));
        assert_eq!(entity_id("vllm").len(), 4 + 32);
    }

    #[test]
    fn recurring_names_are_mentions_sentence_noise_is_not() {
        let body = "However the problem is hard. PyTorch is a framework. We use PyTorch for \
                    training, and PyTorch scales. However we also like NumPy. NumPy is fast.";
        let mentions = extract_mentions(body, 2, 32);
        let keys: Vec<&str> = mentions.iter().map(|m| m.key.as_str()).collect();
        assert!(keys.contains(&"pytorch"), "{keys:?}");
        assert!(keys.contains(&"numpy"), "{keys:?}");
        assert!(!keys.contains(&"however"), "a sentence-initial word seen twice is noise: {keys:?}");
    }

    #[test]
    fn multi_word_phrases_and_boundary_words() {
        // A phrase is captured only when each word is capitalised (`Paged Attention`), which is
        // the documented rule; `Transformer architecture` is one mention, `Transformer`.
        let body = "Paged Attention is a technique. Paged Attention saves memory. BERT and \
                    Paged Attention are used here. BERT is a model. The Rust Language is fast. \
                    The Rust Language is safe.";
        let mentions = extract_mentions(body, 2, 32);
        let keys: Vec<&str> = mentions.iter().map(|m| m.key.as_str()).collect();
        assert!(keys.contains(&"paged attention"), "{keys:?}");
        assert!(keys.contains(&"bert"), "{keys:?}");
        assert!(keys.contains(&"rust language"), "{keys:?}");
        assert!(
            !keys.iter().any(|k| k.starts_with("the ") || k.ends_with(" the")),
            "boundary function words must be trimmed: {keys:?}"
        );

        // A lowercase continuation ends the phrase rather than joining it.
        let body = "Transformer architecture changed everything. Transformer architecture is \
                    used widely.";
        let keys: Vec<String> =
            extract_mentions(body, 2, 32).iter().map(|m| m.key.clone()).collect();
        assert!(keys.contains(&"transformer".to_string()), "{keys:?}");
        assert!(!keys.iter().any(|k| k.contains(' ')), "{keys:?}");
    }

    #[test]
    fn acronyms_and_mixed_case_survive() {
        let body = "vLLM serves models. vLLM is fast. The API is documented. The API is stable.";
        let mentions = extract_mentions(body, 2, 32);
        let keys: Vec<&str> = mentions.iter().map(|m| m.key.as_str()).collect();
        assert!(keys.contains(&"vllm"), "{keys:?}");
        assert!(keys.contains(&"api"), "{keys:?}");
    }

    #[test]
    fn distinct_surface_forms_are_kept_as_one_mention() {
        let body = "vLLM is fast. VLLM and vllm are the same thing here, twice.";
        let mentions = extract_mentions(body, 2, 32);
        let vllm = mentions.iter().find(|m| m.key == "vllm").expect("vLLM must be a mention");
        assert_eq!(vllm.name, "vLLM", "the first form is the display name");
        assert!(vllm.forms.contains(&"VLLM".to_string()), "{:?}", vllm.forms);
        assert_eq!(mentions.iter().filter(|m| m.key == "vllm").count(), 1, "one node per key");
    }

    #[test]
    fn kind_is_lexical_only() {
        assert_eq!(kind_for("Anthropic"), EntityKind::Organization);
        assert_eq!(kind_for("Stanford University"), EntityKind::Organization);
        assert_eq!(kind_for("Paged Attention"), EntityKind::Concept);
        // Never guessed: a bare name is a concept, not a person.
        assert_eq!(kind_for("Sasha Kim"), EntityKind::Concept);
    }

    #[test]
    fn cooccurrence_is_windowed_and_symmetric() {
        let mk = |name: &str, key: &str, first_token: usize| Mention {
            name: name.into(),
            key: key.into(),
            forms: vec![name.into()],
            count: 2,
            first_token,
            last_token: first_token,
        };
        let (a, b, c) = (mk("A", "a", 0), mk("B", "b", 10), mk("C", "c", 500));
        let pairs = cooccurrence_pairs(&[a, b, c], 20);
        assert_eq!(pairs, vec![(("a".to_string(), "b".to_string()), 1.0)]);
    }
}
