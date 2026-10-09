//! `agent0 graph build` — populate the entity / alias / edge / work store from the index
//! (plan Stage 1: "entity aliases + concept graph + source graph").
//!
//! The index is the source of truth: every document's STOREd `title` + `body` is read with
//! [`Pipeline::all_docs`], mentions are extracted with the deterministic rules in
//! `search_graph::extract`, and the result is written through the metadata writer. Nothing here
//! contacts the network or a model, so a build is reproducible from the same index.
//!
//! Three aggregates are produced:
//!
//! * **Entities** — canonical key → one node, with every distinct surface form kept as an alias
//!   (`vLLM` / `VLLM` / `vllm` share one node, three aliases). Entities seen in fewer than
//!   `--min-docs` documents are dropped: a graph of one-off capitalised words is noise, and the
//!   threshold is what buys a usable expansion signal.
//! * **Edges** — mentions co-occurring inside a `--window`-token span in the same document, as
//!   `cooccur` edges weighted by the number of documents in which the pair appears. Weight is a
//!   document count, not a raw occurrence count, so one repetitive page cannot dominate.
//! * **Works** — a DOI / arXiv / PMID detected in the text becomes a `work` row, carrying the
//!   document's title. This is the WorkID resolution Stage 1 asks for: the same paper at a
//!   preprint and a publisher both resolve to one canonical id.

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;

use search_core::config::Config;
use search_graph::{cooccurrence_pairs, entity_id, extract_mentions, kind_for, DEFAULT_COOC_WINDOW};
use search_indexer::IndexDoc;
use search_metadata::{MetadataWriter, Mutation};
use crate::pipeline::Pipeline;

/// Parse `--flag value` for a usize, mirroring the other subcommands.
fn flag_usize(args: &[String], flag: &str) -> Option<usize> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1)?.parse().ok()
}

/// Options for one build, with the same defaults the CLI documents.
#[derive(Debug, Clone)]
pub struct Options {
    limit: usize,
    min_count: usize,
    min_docs: usize,
    max_mentions: usize,
    window: usize,
    batch: usize,
}

impl Options {
    pub fn from_args(args: &[String]) -> Self {
        Self {
            limit: flag_usize(args, "--limit").unwrap_or(usize::MAX),
            // Occurrences of a name inside one document before it is a mention at all.
            min_count: flag_usize(args, "--min-count").unwrap_or(2),
            // Documents a name must appear in before it becomes a node.
            min_docs: flag_usize(args, "--min-docs").unwrap_or(3),
            max_mentions: flag_usize(args, "--max-mentions").unwrap_or(40),
            window: flag_usize(args, "--window").unwrap_or(DEFAULT_COOC_WINDOW),
            batch: flag_usize(args, "--batch").unwrap_or(500),
        }
    }
}

/// Accumulator for one entity across the corpus.
#[derive(Debug, Default)]
struct EntityAgg {
    name: String,
    kind: String,
    docs: BTreeSet<String>,
    aliases: BTreeSet<String>,
}

/// Everything one pass over the corpus produced.
#[derive(Debug, Default)]
struct Built {
    scanned: usize,
    entities: BTreeMap<String, EntityAgg>,
    /// (entity key, entity key) -> documents in which the pair co-occurred.
    edges: BTreeMap<(String, String), BTreeSet<String>>,
    /// work_id -> (id_kind, identifier, title).
    works: BTreeMap<String, (String, String, Option<String>)>,
}

/// One deterministic pass over the documents: mentions, co-occurrence edges, work identifiers.
///
/// Split out from [`run`] so the aggregation can be tested without an index, a database, or a
/// filesystem — the thresholds it applies (`--min-docs`) are the part most likely to be wrong.
fn aggregate(docs: &[IndexDoc], opts: &Options) -> Built {
    let mut out = Built::default();

    for doc in docs.iter().take(opts.limit) {
        out.scanned += 1;
        // Title and headings first: an entity named in the title is a real subject of the page,
        // and the extractor's recurrence rule benefits from the extra occurrences.
        let text = format!("{}\n{}\n{}", doc.title, doc.headings, doc.body);
        let mentions = extract_mentions(&text, opts.min_count, opts.max_mentions);

        // Work identifiers are detected independently of mentions: a paper's landing page may
        // have almost no recurring capitalised name, and its DOI still belongs in the store.
        if let Some((kind, id)) = search_graph::detect_identifier(&text) {
            let title = if doc.title.trim().is_empty() { None } else { Some(doc.title.clone()) };
            out.works.insert(id.clone(), (kind_tag(kind).to_string(), id, title));
        }

        if mentions.is_empty() {
            continue;
        }

        for m in &mentions {
            let entry = out.entities.entry(m.key.clone()).or_insert_with(|| EntityAgg {
                name: m.name.clone(),
                kind: kind_for(&m.name).as_tag().to_string(),
                docs: BTreeSet::new(),
                aliases: BTreeSet::new(),
            });
            entry.docs.insert(doc.doc_id.clone());
            // Every other surface form of the key in this document is an alias: `vLLM` and
            // `VLLM` are one node with two spellings, which is what a query-time resolver needs.
            for form in m.forms.iter().filter(|f| **f != entry.name) {
                entry.aliases.insert(form.clone());
            }
        }

        let keys: BTreeSet<&str> = mentions.iter().map(|m| m.key.as_str()).collect();
        for ((a, b), _weight) in cooccurrence_pairs(&mentions, opts.window) {
            if keys.contains(a.as_str()) && keys.contains(b.as_str()) {
                out.edges.entry((a, b)).or_default().insert(doc.doc_id.clone());
            }
        }

        if let Some((kind, id)) = search_graph::detect_identifier(&text) {
            let title = if doc.title.trim().is_empty() { None } else { Some(doc.title.clone()) };
            out.works.insert(id.clone(), (kind_tag(kind).to_string(), id, title));
        }
    }

    // Keep only entities that recur across documents; drop the edges that point at anything
    // dropped, so the store has no dangling rows.
    out.entities.retain(|_, agg| agg.docs.len() >= opts.min_docs);
    out.edges.retain(|(a, b), _| out.entities.contains_key(a) && out.entities.contains_key(b));
    out
}

/// Build the graph and print a summary. Returns the process exit code.
pub async fn run(args: &[String]) -> ExitCode {
    let opts = Options::from_args(args);
    let cfg = Config::from_env();

    let pipeline = match Pipeline::open(&cfg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to open pipeline: {e}");
            return ExitCode::from(1);
        }
    };
    let docs = match pipeline.all_docs() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("failed to read the index: {e}");
            return ExitCode::from(1);
        }
    };

    let Built { scanned, entities, edges, works } = aggregate(&docs, &opts);

    let mut writer = match MetadataWriter::from_path(&cfg.meta_db()) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("failed to open metadata: {e}");
            return ExitCode::from(1);
        }
    };

    let mut pending: Vec<Mutation> = Vec::new();
    let mut written_entities = 0usize;
    let mut written_aliases = 0usize;

    for (key, agg) in &entities {
        let id = entity_id(key);
        pending.push(Mutation::UpsertEntity {
            entity_id: id.clone(),
            name: agg.name.clone(),
            kind: agg.kind.clone(),
        });
        written_entities += 1;
        for alias in &agg.aliases {
            pending.push(Mutation::UpsertEntityAlias { entity_id: id.clone(), alias: alias.clone() });
            written_aliases += 1;
        }
        if pending.len() >= opts.batch {
            if let Err(e) = writer.apply_batch(&std::mem::take(&mut pending)) {
                eprintln!("failed to write entities: {e}");
                return ExitCode::from(1);
            }
        }
    }

    // Edges and works need their endpoints to exist first (the schema has FKs), so they are
    // written after the entity pass has been committed.
    if let Err(e) = writer.apply_batch(&std::mem::take(&mut pending)) {
        eprintln!("failed to write entities: {e}");
        return ExitCode::from(1);
    }

    let mut written_edges = 0usize;
    for ((a, b), docs) in &edges {
        pending.push(Mutation::UpsertGraphEdge {
            from_id: entity_id(a),
            to_id: entity_id(b),
            kind: "cooccur".to_string(),
            weight: docs.len() as f64,
        });
        written_edges += 1;
        if pending.len() >= opts.batch {
            if let Err(e) = writer.apply_batch(&std::mem::take(&mut pending)) {
                eprintln!("failed to write edges: {e}");
                return ExitCode::from(1);
            }
        }
    }
    let mut written_works = 0usize;
    for (_, (kind, identifier, title)) in &works {
        pending.push(Mutation::UpsertWork {
            work_id: identifier.clone(),
            id_kind: kind.clone(),
            identifier: identifier.clone(),
            title: title.clone(),
            published_at: None,
            modified_at: None,
            resource_id: None,
        });
        written_works += 1;
    }
    if let Err(e) = writer.apply_batch(&pending) {
        eprintln!("failed to write edges and works: {e}");
        return ExitCode::from(1);
    }

    println!("graph build: {} document(s) scanned", scanned);
    println!(
        "  entities: {written_entities} (aliases {written_aliases}); dropped as too rare: kept threshold --min-docs {}",
        opts.min_docs
    );
    println!("  edges (cooccur, weight = document count): {written_edges}");
    println!("  works (doi/arxiv/pmid): {written_works}");
    println!("  thresholds: --min-count {} --window {} --max-mentions {}", opts.min_count, opts.window, opts.max_mentions);
    ExitCode::SUCCESS
}

fn kind_tag(kind: search_graph::WorkIdKind) -> &'static str {
    match kind {
        search_graph::WorkIdKind::Doi => "doi",
        search_graph::WorkIdKind::Arxiv => "arxiv",
        search_graph::WorkIdKind::Pmid => "pmid",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: &str, title: &str, body: &str) -> IndexDoc {
        IndexDoc::new(
            id,
            format!("res_{id}"),
            format!("https://example.com/{id}"),
            title,
            body,
        )
    }

    fn opts(min_docs: usize) -> Options {
        Options { limit: usize::MAX, min_count: 2, min_docs, max_mentions: 40, window: 50, batch: 10 }
    }

    #[test]
    fn recurring_entities_become_nodes_with_aliases() {
        let docs = vec![
            doc("d1", "vLLM Paged Attention", "vLLM implements Paged Attention. vLLM is fast."),
            doc("d2", "vLLM notes", "vLLM again. VLLM spelled differently. Paged Attention too."),
            doc("d3", "unrelated", "nothing here twice"),
        ];
        let built = aggregate(&docs, &opts(2));
        assert_eq!(built.scanned, 3);
        assert!(built.entities.contains_key("vllm"), "{:?}", built.entities.keys());
        let vllm = &built.entities["vllm"];
        assert_eq!(vllm.docs.len(), 2);
        assert!(
            vllm.aliases.contains("VLLM"),
            "a different surface form must be stored as an alias: {:?}",
            vllm.aliases
        );
    }

    #[test]
    fn a_name_in_one_document_only_is_dropped() {
        // "Paged Attention" appears in two docs, "Zeta Kernel" in one: the threshold is what
        // keeps a graph useful rather than a list of every capitalised word.
        let docs = vec![
            doc("d1", "Paged Attention", "Paged Attention is used. Paged Attention is fast."),
            doc("d2", "Paged Attention", "Paged Attention again. Paged Attention here."),
            doc("d3", "Zeta Kernel", "Zeta Kernel is new. Zeta Kernel is experimental."),
        ];
        let built = aggregate(&docs, &opts(2));
        assert!(built.entities.contains_key("paged attention"));
        assert!(
            !built.entities.contains_key("zeta kernel"),
            "an entity in fewer than --min-docs documents must not become a node"
        );
    }

    #[test]
    fn edge_weight_counts_documents_not_occurrences() {
        let docs = vec![
            doc("d1", "A", "Paged Attention and vLLM together. Paged Attention, vLLM, again."),
            doc("d2", "B", "Paged Attention with vLLM. Paged Attention and vLLM. vLLM."),
            doc("d3", "C", "Paged Attention alone. Paged Attention only."),
        ];
        let built = aggregate(&docs, &opts(2));
        let key = ("paged attention".to_string(), "vllm".to_string());
        let weight = built.edges.get(&key).map(|d| d.len()).unwrap_or(0);
        assert_eq!(
            weight, 2,
            "the weight must be the number of documents in which the pair co-occurs: {:?}",
            built.edges
        );
    }

    #[test]
    fn a_detected_identifier_becomes_a_work() {
        let docs = vec![
            doc("d1", "Paged Attention", "See arXiv:2301.00001 for details. arXiv:2301.00001."),
            doc(
                "d2",
                "Retrieval",
                "Published at https://doi.org/10.1234/abcd. Paged Attention cited it too.",
            ),
        ];
        let built = aggregate(&docs, &opts(1));
        assert!(built.works.contains_key("arxiv:2301.00001"), "{:?}", built.works.keys());
        assert!(built.works.contains_key("doi:10.1234/abcd"), "{:?}", built.works.keys());
        assert_eq!(built.works["arxiv:2301.00001"].0, "arxiv");
        assert_eq!(built.works["arxiv:2301.00001"].2.as_deref(), Some("Paged Attention"));
    }
}
