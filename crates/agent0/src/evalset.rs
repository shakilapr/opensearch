//! Build a labelled query set from the live corpus (work plan, Stage 2).
//!
//! Stage 2 asks for 500–2000 labelled queries; the repository had a 25-case hand-judged smoke set
//! over a 30-document fixture, which cannot support a quality claim about a 19 000-document index.
//! This derives cases from the corpus itself: **the query is a document's title and the label is
//! that document**.
//!
//! What that measures, stated plainly so nobody mistakes it for more: whether the engine can find
//! the page a title describes. It is a *proxy* for human relevance judgements, and it inherits
//! three biases worth knowing —
//!
//! * one known-relevant document per query, so it cannot measure recall of a *set*;
//! * title-shaped queries, which are shorter and cleaner than real ones;
//! * the corpus's own selection (whatever was crawled is the ground truth).
//!
//! What it does buy is scale and reproducibility: hundreds of cases, derived mechanically, so a
//! ranking change can be measured instead of asserted. The rules that keep the proxy honest are
//! enforced here rather than left to the reader:
//!
//! * a title has to be *query-like* — not a one-word label, not a paragraph;
//! * a duplicated title is **excluded**, because two documents with the same title break the
//!   single-label assumption (nothing says which one is "the" answer);
//! * a document with almost no text is excluded, since it cannot be retrieved by content at all;
//! * the sample is **strided** across the doc-id order, not the first N, so it does not become a
//!   survey of whichever host sorts first;
//! * cases are ordered deterministically, so regenerating gives the same file.

use std::collections::HashMap;
use std::path::PathBuf;

use search_eval::{QueryCase, QueryClass};
use search_indexer::doc::IndexDoc;

/// Defaults chosen so the resulting queries look like things a person types.
const DEFAULT_SAMPLE: usize = 600;
const DEFAULT_MIN_TITLE_WORDS: usize = 3;
const DEFAULT_MAX_TITLE_WORDS: usize = 14;
/// Below this the document is a stub — a title match would say nothing about retrieval quality.
const DEFAULT_MIN_BODY_CHARS: usize = 400;
/// Titles at or below this length are classed `ShortFactual`, longer ones `LongSemantic`.
const SHORT_TITLE_WORDS: usize = 6;

pub struct Options {
    pub out: PathBuf,
    pub sample: usize,
    pub min_title_words: usize,
    pub max_title_words: usize,
    pub min_body_chars: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            out: PathBuf::from("benchmarks/queries.corpus.jsonl"),
            sample: DEFAULT_SAMPLE,
            min_title_words: DEFAULT_MIN_TITLE_WORDS,
            max_title_words: DEFAULT_MAX_TITLE_WORDS,
            min_body_chars: DEFAULT_MIN_BODY_CHARS,
        }
    }
}

impl Options {
    pub fn from_args(args: &[String]) -> Self {
        let mut opts = Options::default();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--out" => {
                    if let Some(v) = args.get(i + 1) {
                        opts.out = PathBuf::from(v);
                    }
                    i += 2;
                }
                "--sample" => {
                    if let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) {
                        opts.sample = v;
                    }
                    i += 2;
                }
                "--min-title-words" => {
                    if let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) {
                        opts.min_title_words = v;
                    }
                    i += 2;
                }
                "--max-title-words" => {
                    if let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) {
                        opts.max_title_words = v;
                    }
                    i += 2;
                }
                "--min-body-chars" => {
                    if let Some(v) = args.get(i + 1).and_then(|v| v.parse().ok()) {
                        opts.min_body_chars = v;
                    }
                    i += 2;
                }
                _ => i += 1,
            }
        }
        opts
    }
}

/// Derive cases from stored documents. Pure, so the rules above are testable without an index.
pub fn cases_from_docs(docs: &[IndexDoc], opts: &Options) -> Vec<QueryCase> {
    // Deterministic input order: the caller's order is not guaranteed to be stable across runs.
    let mut candidates: Vec<&IndexDoc> = docs.iter().filter(|d| is_query_like(d, opts)).collect();
    candidates.sort_by(|a, b| a.doc_id.cmp(&b.doc_id));

    // A title two documents share cannot support a single-label case.
    let mut title_counts: HashMap<String, usize> = HashMap::new();
    for doc in &candidates {
        *title_counts.entry(normalise_title(&doc.title)).or_insert(0) += 1;
    }
    let unique: Vec<&IndexDoc> = candidates
        .into_iter()
        .filter(|d| title_counts.get(&normalise_title(&d.title)).copied().unwrap_or(0) == 1)
        .collect();

    if unique.is_empty() || opts.sample == 0 {
        return Vec::new();
    }

    // Stride through the sorted list so the sample spans the corpus rather than its first host.
    let take = opts.sample.min(unique.len());
    let step = unique.len() as f64 / take as f64;
    let mut cases = Vec::with_capacity(take);
    for i in 0..take {
        let doc = unique[(i as f64 * step) as usize];
        let query = normalise_title(&doc.title);
        let words = query.split_whitespace().count();
        cases.push(QueryCase {
            id: format!("corpus-{i:05}"),
            query,
            class: if words <= SHORT_TITLE_WORDS {
                QueryClass::ShortFactual
            } else {
                QueryClass::LongSemantic
            },
            // One known-relevant document; the graded form carries it as highly relevant.
            relevant: vec![doc.doc_id.clone()],
            graded: vec![(doc.doc_id.clone(), 3)],
            observed_at: Some(doc.fetched_at),
        });
    }
    cases
}

fn is_query_like(doc: &IndexDoc, opts: &Options) -> bool {
    // A tombstoned document is not served, so labelling it relevant would count every query as a
    // miss. (Watermark gating is not consulted: a settled corpus has all live docs published.)
    if doc.tombstone {
        return false;
    }
    let title = normalise_title(&doc.title);
    let words = title.split_whitespace().count();
    if words < opts.min_title_words || words > opts.max_title_words {
        return false;
    }
    if doc.body.chars().count() < opts.min_body_chars {
        return false;
    }
    // A title with no letters is a code, a date or a number — not a query.
    title.chars().any(|c| c.is_alphabetic())
}

/// Collapse whitespace so a title used as a query is a single clean line.
fn normalise_title(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub async fn run(args: &[String]) -> std::process::ExitCode {
    use search_core::config::Config;

    let opts = Options::from_args(args);
    let cfg = Config::from_env();

    // Read-only: building an eval set must not take the writer lock from a running server or crawl.
    let pipeline = match super::pipeline::Pipeline::open_read_only(&cfg, false) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to open the index: {e}");
            return std::process::ExitCode::from(1);
        }
    };
    let docs = match pipeline.all_docs() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("failed to read the index: {e}");
            return std::process::ExitCode::from(1);
        }
    };

    let total = docs.len();
    let cases = cases_from_docs(&docs, &opts);

    let mut body = String::new();
    body.push_str(
        "# Corpus-derived query set (Stage 2). NOT hand-judged.\n\
         # Each case's query is a document title and the label is that document: a proxy for\n\
         # \"can the engine find the page a title describes\", with one known-relevant document per\n\
         # query. Regenerate with: agent0 eval build-set\n",
    );
    for case in &cases {
        match serde_json::to_string(case) {
            Ok(line) => {
                body.push_str(&line);
                body.push('\n');
            }
            Err(e) => {
                eprintln!("failed to serialise a case: {e}");
                return std::process::ExitCode::from(1);
            }
        }
    }

    if let Some(parent) = opts.out.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("failed to create {}: {e}", parent.display());
                return std::process::ExitCode::from(1);
            }
        }
    }
    if let Err(e) = std::fs::write(&opts.out, body) {
        eprintln!("failed to write {}: {e}", opts.out.display());
        return std::process::ExitCode::from(1);
    }

    println!("eval build-set: {total} document(s) in the index");
    println!("  cases        : {} (sample target {})", cases.len(), opts.sample);
    println!(
        "  rules        : title {}-{} words, body >= {} chars, unique titles only",
        opts.min_title_words, opts.max_title_words, opts.min_body_chars
    );
    println!("  labels       : corpus-derived (title → its document); not human judgements");
    println!("  output       : {}", opts.out.display());
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: &str, title: &str, body_chars: usize) -> IndexDoc {
        let mut d = IndexDoc::new(
            id,
            format!("res-{id}"),
            format!("https://example.test/{id}"),
            title,
            "x".repeat(body_chars),
        );
        d.fetched_at = 1_700_000_000;
        d
    }

    fn opts(sample: usize) -> Options {
        Options { sample, ..Options::default() }
    }

    #[test]
    fn a_title_becomes_a_query_labelled_with_its_own_document() {
        let docs = vec![doc("d1", "Paged Attention in vLLM", 900), doc("d2", "Vector Databases Explained Simply", 900)];
        let cases = cases_from_docs(&docs, &opts(10));
        assert_eq!(cases.len(), 2);
        let first = &cases[0];
        assert_eq!(first.class, QueryClass::ShortFactual);
        assert_eq!(first.relevant, vec!["d1".to_string()]);
        assert_eq!(first.graded, vec![("d1".to_string(), 3)]);
        assert_eq!(first.observed_at, Some(1_700_000_000));
    }

    #[test]
    fn a_duplicated_title_is_excluded_because_the_label_would_be_ambiguous() {
        let docs = vec![
            doc("d1", "Rust Ownership Rules", 900),
            doc("d2", "Rust Ownership Rules", 900),
            doc("d3", "Something Else Entirely", 900),
        ];
        let cases = cases_from_docs(&docs, &opts(10));
        assert_eq!(cases.len(), 1, "both copies of the shared title must be dropped");
        assert_eq!(cases[0].relevant, vec!["d3".to_string()]);
    }

    #[test]
    fn query_like_titles_only() {
        let docs = vec![
            doc("d1", "Rust", 900),                                             // too short
            doc("d2", &"word ".repeat(20), 900),                                 // too long
            doc("d3", "A Reasonable Title Here", 10),                            // body is a stub
            doc("d4", "Another Reasonable Title", 900),                          // kept
            doc("d5", "12345 67890 111213", 900),                                // no letters
        ];
        let cases = cases_from_docs(&docs, &opts(10));
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].relevant, vec!["d4".to_string()]);
    }

    #[test]
    fn the_sample_is_strided_across_the_corpus_not_taken_from_the_front() {
        let docs: Vec<IndexDoc> =
            (0..40).map(|i| doc(&format!("d{i:03}"), &format!("Title Number {i} Here"), 900)).collect();
        let cases = cases_from_docs(&docs, &opts(4));
        assert_eq!(cases.len(), 4);
        // Selected ids must be spread over the whole range, not the first four. (With 40 documents
        // and a sample of 4 the stride is 10, so the last pick is d030 — the final decile, which is
        // the property that matters; the earlier `> "d030"` expectation was simply wrong.)
        let ids: Vec<&str> = cases.iter().map(|c| c.relevant[0].as_str()).collect();
        assert!(ids.contains(&"d000"), "{ids:?}");
        assert!(ids.iter().any(|id| *id >= "d030"), "the sample must reach the far end: {ids:?}");
        assert!(!ids.contains(&"d001") && !ids.contains(&"d002"), "not the front of the list: {ids:?}");
    }

    #[test]
    fn regeneration_is_deterministic() {
        let docs: Vec<IndexDoc> =
            (0..20).map(|i| doc(&format!("d{i:03}"), &format!("Title Number {i} Here"), 900)).collect();
        assert_eq!(cases_from_docs(&docs, &opts(7)), cases_from_docs(&docs, &opts(7)));
    }

    #[test]
    fn whitespace_in_a_title_is_collapsed() {
        let docs = vec![doc("d1", "  Spaced   Out   Title  ", 900)];
        assert_eq!(cases_from_docs(&docs, &opts(5))[0].query, "Spaced Out Title");
    }

    #[test]
    fn a_tombstoned_document_is_never_labelled() {
        // It is not served, so a case built from it would count every query as a miss.
        let mut dead = doc("d1", "A Deleted Page Title", 900);
        dead.tombstone = true;
        let docs = vec![dead, doc("d2", "A Live Page Title Here", 900)];
        let cases = cases_from_docs(&docs, &opts(10));
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].relevant, vec!["d2".to_string()]);
    }

    #[test]
    fn an_empty_corpus_yields_no_cases() {
        assert!(cases_from_docs(&[], &opts(10)).is_empty());
        let docs = vec![doc("d1", "A Valid Title Here", 900)];
        assert!(cases_from_docs(&docs, &opts(0)).is_empty(), "a zero sample asks for nothing");
    }
}
