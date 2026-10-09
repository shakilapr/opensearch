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
    if !first.is_alphabetic() {
        // Number-led tokens are units, versions and hex literals far more often than names
        // ("10MB", "0x00", "128k", "16GB" — all observed as the top "entities" of a real
        // build). Requiring a leading letter keeps `GPT-4`, `B2B` and `R2R` and drops those.
        return false;
    }
    let uppercase = token.chars().filter(|c| c.is_uppercase()).count();
    let lowercase_after_first = token.chars().skip(1).any(|c| c.is_lowercase());
    first.is_uppercase()                    // Rust, Kubernetes, Anthropic
        || uppercase >= 2                   // vLLM, iOS, API, GPT
        || (lowercase_after_first && token.chars().any(|c| c.is_ascii_digit())) // token2vec
}

/// Words that are document *structure*, not entities.
///
/// A page's navigation and headings repeat on every page of a site, so they clear any
/// recurrence threshold and then dominate the graph ("Engine", "Release", "Image", "Storage"
/// produced the highest-weight edges of a real build). They only apply to single-word mentions:
/// a multi-word phrase ("Release Notes" aside) or an acronym is kept, because that is where the
/// signal actually is.
const STRUCTURAL_WORDS: &[&str] = &[
    "engine", "release", "releases", "image", "images", "storage", "level", "queue", "pub",
    "notes", "overview", "guide", "reference", "index", "docs", "documentation", "home", "page",
    "pages", "search", "results", "input", "output", "data", "client", "server", "tools", "tool",
    "usage", "example", "examples", "tutorial", "tutorials", "changelog", "license", "about",
    "blog", "news", "events", "pricing", "support", "help", "login", "signup", "privacy",
    "terms", "cookies", "menu", "skip", "content", "main", "sidebar", "footer", "header",
    "title", "section", "chapter", "table", "figure", "next", "previous", "back", "top", "more",
    "read", "share", "comments", "subscribe", "newsletter", "download", "downloads", "install",
    "installation", "getting", "started", "quickstart", "configuration", "config", "settings",
    "options", "parameters", "arguments", "flags", "commands", "command", "syntax", "return",
    "returns", "value", "values", "type", "types", "name", "names", "id", "ids", "key", "keys",
    "true", "false", "null", "none", "error", "errors", "warning", "warnings", "note",
    "important", "tip", "summary", "description", "details", "introduction", "conclusion",
    "appendix", "contents", "collections", "collection", "default", "defaults", "general",
    "advanced", "basic", "custom", "common", "latest", "new", "old", "file", "files", "folder",
    "path", "paths", "size", "length", "count", "number", "string", "format", "version",
    "versions", "update", "updates", "change", "changes", "log", "logs", "code", "language",
    "project", "projects", "team", "team.", "user", "users", "account", "accounts", "session",
];

fn is_structural_word(key: &str) -> bool {
    !key.contains(' ') && STRUCTURAL_WORDS.contains(&key)
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
    // Built once per document: rebuilding it per candidate made the pass quadratic and a
    // 11 800-document build took longer than seven minutes.
    let joined = doc_lower.join(" ");
    let joined_bytes = joined.as_bytes();

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
        if is_structural_word(&key) {
            continue;
        }
        // Count occurrences over the space-joined canonical text with explicit word boundaries,
        // advancing by one character per attempt so a repeated word is counted every time
        // (" numpy numpy " is two occurrences, not one) and a multi-token key still matches a
        // single token whose canonical form contains the separator (`GPT-4` → "gpt 4").
        let count = {
            let bytes = joined_bytes;
            let mut count = 0usize;
            let mut from = 0usize;
            while from <= joined.len() {
                let Some(rel) = joined[from..].find(&key) else { break };
                let pos = from + rel;
                let end = pos + key.len();
                let before_ok = pos == 0 || bytes[pos - 1] == b' ';
                let after_ok = end == joined.len() || bytes[end] == b' ';
                if before_ok && after_ok {
                    count += 1;
                }
                // Advance by one *character*, not one byte: the corpus contains multibyte text,
                // and slicing at a mid-character byte index panics ("start byte index 617 is not
                // a char boundary; it is inside 'р'") — hit live on a Cyrillic page.
                let step = joined[pos..].chars().next().map(char::len_utf8).unwrap_or(1);
                from = pos + step;
            }
            count
        };
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
    fn numeric_tokens_and_structural_words_are_not_entities() {
        // All of these were top "entities" of a real build: doc units (`10MB`, `0x00`), and a
        // site's own navigation words repeated on every page (`Engine`, `Release`, `Image`).
        let body = "10MB and 0x00 and 16GB are sizes. Engine and Release and Image appear \
                    twice. Engine and Release and Image again. GPT-4 is a model. GPT-4 is used.";
        let keys: Vec<String> =
            extract_mentions(body, 2, 32).iter().map(|m| m.key.clone()).collect();
        for bad in ["10mb", "0x00", "16gb", "engine", "release", "image"] {
            assert!(!keys.contains(&bad.to_string()), "{bad} must not be an entity: {keys:?}");
        }
        assert!(keys.contains(&"gpt 4".to_string()), "a letter-led token with a digit stays: {keys:?}");
    }

    #[test]
    fn multibyte_text_does_not_panic_the_occurrence_counter() {
        // A live build panicked here ("start byte index 617 is not a char boundary; it is inside
        // 'р'") because the counter advanced by one *byte*. Cyrillic, CJK and accented text must
        // all be safe.
        let body = "Антропология и Anthropic. Антропология снова. Anthropic является компанией. \
                    Anthropic снова. 日本語のテスト。日本語のテスト。";
        let keys: Vec<String> =
            extract_mentions(body, 2, 32).iter().map(|m| m.key.clone()).collect();
        assert!(keys.contains(&"anthropic".to_string()), "{keys:?}");
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
