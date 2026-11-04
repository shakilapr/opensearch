# Task 01: Dynamic Open Concept Vocabulary System

## Objective
Break the hardcoded 15-concept constraint in `crates/retrieval/src/concept_tag.rs` and `crates/agent0/src/conceptbuild.rs` to allow the concept universe to scale to thousands/millions of dynamic concepts.

## Requirements

### 1. `crates/retrieval/src/concept_tag.rs`
- Make `ConceptSeed` support owned strings (`String`) or flexible lifetimes so concepts can be loaded dynamically from files, databases, or API responses at runtime.
- Retain `pub const DEFAULT_SEEDS: &[...]` (or `DEFAULT_LEXICON`) with the original 15 seeds for backwards compatibility.
- Add a `ConceptLexicon` struct (or dynamic loader) that:
  - Can be instantiated from `DEFAULT_SEEDS` or loaded from JSON (`from_json_str` / `from_reader`).
  - Provides a method `tag(&self, title: &str, headings: &str, body: &str) -> Vec<ConceptAssociation>` against all registered concepts.
  - Retains the existing standalone `tag(title, headings, body, seeds)` helper for backwards compatibility.
- Add comprehensive unit tests in `crates/retrieval/src/concept_tag.rs` proving dynamic concepts can be loaded from JSON and correctly tag sample documents.

### 2. `crates/agent0/src/conceptbuild.rs`
- Update `register_lexicon` and `agent0 concepts build` CLI:
  - Add `--lexicon <PATH>` flag allowing users to pass an external JSON file containing thousands of concept definitions.
  - If `--lexicon` is not passed, default to the built-in seeds.
  - Pass the dynamic lexicon to `tag()` during batch tagging.

### 3. Verification & Safety Criteria
- `export PATH="$HOME/.cargo/bin:$PATH"`
- `cargo check --workspace`
- `cargo test -p search-retrieval -p search-agent0`
- All existing tests must pass.
- Conventional git commit: `feat(retrieval): support dynamic open concept vocabularies`
