//! One deterministic tokenizer, shared by the lexical cross-encoder and the disambiguator so
//! both judge "overlap" the same way.
//!
//! Rule: lowercase, split on every non-alphanumeric character, drop empty pieces. No stemming,
//! no stop-word removal — reproducibility beats cleverness for a default lane.

/// Lowercase alphanumeric tokens.
pub fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::tokenize;

    #[test]
    fn splits_on_punctuation_and_case() {
        assert_eq!(tokenize("Apple, Inc. (Cupertino)!"), ["apple", "inc", "cupertino"]);
        assert!(tokenize("   ---  ").is_empty());
        assert_eq!(tokenize("a1b2"), ["a1b2"]);
    }
}
