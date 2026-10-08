//! Word k-shingling.

use std::collections::BTreeSet;

/// Split `text` into lower-cased alphanumeric word tokens, then return its set of overlapping
/// `k`-word shingles (each joined by a single space).
///
/// * `k == 0`, or fewer than `k` tokens, yields an empty set.
/// * Tokenization is Unicode-aware (`char::is_alphanumeric`) and case-folded, so `"The cat"`
///   and `"the cat"` shingle identically.
pub fn shingles(text: &str, k: usize) -> BTreeSet<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            cur.extend(ch.to_lowercase());
        } else if !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }

    let mut out = BTreeSet::new();
    if k == 0 || tokens.len() < k {
        return out;
    }
    for window in tokens.windows(k) {
        out.insert(window.join(" "));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shingles_are_overlapping_kgrams() {
        let s = shingles("The quick brown fox", 2);
        let got: Vec<&str> = s.iter().map(String::as_str).collect();
        assert_eq!(got, vec!["brown fox", "quick brown", "the quick"]);
    }

    #[test]
    fn shingles_lowercase_and_punctuation_insensitive() {
        assert_eq!(
            shingles("Hello, WORLD!", 2),
            shingles("hello world", 2),
            "punctuation and case must not change the shingle set"
        );
    }

    #[test]
    fn shingles_degenerate_k_is_empty() {
        assert!(shingles("a b c", 0).is_empty());
        assert!(shingles("a b", 5).is_empty());
        assert_eq!(shingles("solo", 1), BTreeSet::from(["solo".to_string()]));
    }
}
