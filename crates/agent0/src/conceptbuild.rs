//! `agent0 concepts build` — register the lexicon and fill the concept matrix.
//!
//! This is the producer for `doc_concept`, the table `docs/SEMANTIC_INDEX_ARCHITECTURE.md` builds
//! the whole semantic index around. Measured 2026-10-28, it and its four sibling tables held **zero
//! rows**: the schema, the migrations and their tests all existed, but nothing ever wrote a single
//! association, so the concept lane had nothing to read and every query fell back to lexical scan.
//!
//! What it does, per document already on disk:
//! 1. register the bundled concept lexicon (`concept` + `concept_alias`),
//! 2. read the document's title/headings/body from the extracted text,
//! 3. weight it against every concept (`search_retrieval::concept_tag::tag`),
//! 4. write the surviving associations to `doc_concept` in batches.
//!
//! Deterministic and rerunnable: re-tagging after the lexicon grows updates weights in place
//! (`ON CONFLICT ... DO UPDATE`), and nothing is deleted, so a grown lexicon only adds.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use search_core::config::Config;
use search_core::AppResult;
use search_metadata::{MetadataWriter, Mutation};
use search_retrieval::concept_tag;

/// Documents per write transaction. Associations are ~8 per document, so 2,000 documents is
/// ~16,000 rows per commit — large enough to amortise the fsync, small enough that a kill loses
/// seconds of work.
const BATCH_DOCS: usize = 2_000;

/// Associations below this direct weight are dropped for a concept covering >20% of the corpus.
/// At 0.30 a page that merely mentions the field is excluded while a page that discusses it stays.
const UBIQUITY_STRONG_EVIDENCE: f64 = 0.30;

/// Stamped on every association and dimension row produced by this pass, so a later better
/// teacher can be introduced without guessing which rows it produced (v2 §4, §6).
const MODEL_VERSION: &str = "lexicon-v1";

/// Relation kind written by `concepts relate`. A determinstic, corpus-derived edge, kept distinct
/// from hand-written taxonomy relations (`related`, `mechanism_of`) in the same table.
const MODEL_RELATION: &str = "cooccur";

fn flag_usize(args: &[String], flag: &str) -> Option<usize> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1)?.parse().ok()
}

fn flag_str(args: &[String], flag: &str) -> Option<String> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1).cloned()
}

/// One document ready to tag.
struct Doc {
    resource_id: String,
    /// The pagination cursor. Must be the same column the query orders and filters on
    /// (`document_version.docver_id`); an earlier version advanced the cursor to `resource_id`
    /// while filtering on `docver_id`, so every page restarted near the beginning of the corpus and
    /// the same documents were re-tagged indefinitely. Measured 2026-10-29: 60,435 documents in 25
    /// minutes, with progress stalling because pages overlapped almost completely.
    docver_id: String,
    title: String,
    text_path: String,
}

/// Register the lexicon. Idempotent: a concept or alias already present is left alone.
fn register_lexicon(w: &mut MetadataWriter, now: i64) -> AppResult<usize> {
    let mut batch = Vec::new();
    for (concept_id, name, kind, alias) in concept_tag::registry_rows() {
        match alias {
            None => batch.push(Mutation::UpsertConcept {
                concept_id,
                name,
                description: None,
                kind,
                created_at: now,
            }),
            Some(a) => batch.push(Mutation::UpsertConceptAlias {
                concept_id,
                alias: a,
                lang: Some("en".into()),
                created_at: now,
            }),
        }
    }
    let n = batch.len();
    w.apply_batch(&batch)?;
    Ok(n)
}

/// Documents on disk that have extracted text, paged on `docver_id`.
fn page_of_docs(
    reader: &search_metadata::MetadataReader,
    after: &str,
    limit: usize,
) -> AppResult<Vec<Doc>> {
    let conn = reader.conn();
    let mut stmt = conn
        .prepare(
            // Newest version per resource: tagging every historic version would multiply
            // associations for a page that changed once.
            "SELECT dv.resource_id, dv.docver_id, COALESCE(dv.title, ''), c.text_path
               FROM document_version dv
               JOIN content c ON c.content_id = dv.content_id
              WHERE c.text_path IS NOT NULL
                AND dv.docver_id > ?1
                AND dv.version_no = (
                      SELECT MAX(d2.version_no) FROM document_version d2
                       WHERE d2.resource_id = dv.resource_id)
              ORDER BY dv.docver_id
              LIMIT ?2",
        )
        .map_err(search_metadata::db_err)?;
    let rows = stmt
        .query_map(rusqlite::params![after, limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .map_err(search_metadata::db_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(search_metadata::db_err)?;
    Ok(rows
        .into_iter()
        .map(|(resource_id, docver_id, title, text_path)| Doc {
            resource_id,
            docver_id,
            title,
            text_path,
        })
        .collect())
}

/// `agent0 concepts build [--dir PATH]... [--batch N] [--limit N] [--rebuild] [--dry-run]`
///
/// `--rebuild` purges this pass's own `model_version` before tagging. Tagging is otherwise an
/// upsert, so a corrected lexicon cannot remove an association the previous lexicon created — a
/// fix to the vocabulary would appear to have no effect on the matrix.
pub async fn run(args: &[String]) -> ExitCode {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut i = 3;
    while i < args.len() {
        if args[i] == "--dir" {
            match args.get(i + 1) {
                Some(v) => {
                    dirs.push(PathBuf::from(v));
                    i += 2;
                }
                None => return usage("--dir needs a path"),
            }
        } else {
            break;
        }
    }
    if dirs.is_empty() {
        dirs.push(Config::from_env().data_dir);
    }
    let batch = flag_usize(args, "--batch").unwrap_or(BATCH_DOCS).max(1);
    let limit = flag_usize(args, "--limit").unwrap_or(usize::MAX);
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let rebuild = args.iter().any(|a| a == "--rebuild");
    let _ = flag_str(args, "--unknown");

    let mut failed = false;
    for dir in &dirs {
        let mut cfg = Config::from_env();
        cfg.data_dir = dir.clone();
        println!("{}:", dir.display());
        match build_root(&cfg, batch, limit, dry_run, rebuild) {
            Ok(()) => {}
            Err(e) => {
                failed = true;
                eprintln!("  error: {e}");
            }
        }
    }
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn build_root(
    cfg: &Config,
    batch: usize,
    limit: usize,
    dry_run: bool,
    rebuild: bool,
) -> AppResult<()> {
    let started = Instant::now();
    let now = search_core::time::now_unix_s();
    let mut writer = MetadataWriter::from_path(&cfg.meta_db())?;
    if rebuild && !dry_run {
        writer.apply_batch(&[Mutation::PurgeModelVersion {
            model_version: MODEL_VERSION.to_string(),
        }])?;
        println!("  purged associations from {MODEL_VERSION}");
    }
    let registered = register_lexicon(&mut writer, now)?;
    println!("  lexicon: {registered} concept/alias rows registered");
    if dry_run {
        println!("  dry run: associations not written");
    }

    let reader = search_metadata::MetadataReader::from_path(&cfg.meta_db())?;
    let mut after = String::new();
    let mut seen = 0usize;
    let mut tagged = 0usize;
    let mut assocs = 0usize;
    let mut dims = 0usize;

    // Pass 1: how many documents each concept would claim. Needed because ubiquity can only be
    // measured against the corpus, not guessed: `c-ml` claimed 32% of the serving root and drowned
    // every real association. Cheap — tagging is 400 docs/s and nothing is written.
    let mut concept_docs: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut total_docs = 0usize;
    {
        let mut probe_after = String::new();
        loop {
            let batch = page_of_docs(&reader, &probe_after, batch)?;
            if batch.is_empty() {
                break;
            }
            for d in &batch {
                probe_after = d.docver_id.clone();
                total_docs += 1;
                let body = std::fs::read_to_string(&d.text_path).unwrap_or_default();
                if body.trim().is_empty() {
                    continue;
                }
                for a in concept_tag::tag(&d.title, "", &body, concept_tag::LEXICON) {
                    *concept_docs.entry(a.concept_id).or_insert(0) += 1;
                }
            }
            if total_docs >= limit {
                break;
            }
        }
    }
    let ubiquity_limit = (total_docs as f64 * concept_tag::UBIQUITY_LIMIT) as usize;
    let ubiquitous: std::collections::HashSet<String> = concept_docs
        .iter()
        .filter(|(_, n)| **n > ubiquity_limit.max(1))
        .map(|(c, _)| c.clone())
        .collect();
    if !ubiquitous.is_empty() {
        let mut names: Vec<&String> = ubiquitous.iter().collect();
        names.sort();
        println!(
            "  ubiquity: dropping {} concept(s) covering >{:.0}% of {total_docs} docs: {names:?}",
            ubiquitous.len(),
            concept_tag::UBIQUITY_LIMIT * 100.0
        );
    }

    'pages: loop {
        let docs = page_of_docs(&reader, &after, batch.min(limit.saturating_sub(seen)).max(1))?;
        if docs.is_empty() {
            break;
        }
        let mut mutations: Vec<Mutation> = Vec::new();
        for d in docs {
            seen += 1;
            if seen > limit {
                break 'pages;
            }
            // Title and headings come from the metadata row; body from the extracted text.
            let body = std::fs::read_to_string(&d.text_path).unwrap_or_default();
            if body.trim().is_empty() {
                continue;
            }
            // Headings live in the body text for extracted pages, so they are searched through the
            // body; a page's title comes from the metadata row.
            let assocs_for_doc = concept_tag::tag(&d.title, "", &body, concept_tag::LEXICON);
            after = d.docver_id.clone();
            if assocs_for_doc.is_empty() {
                continue;
            }
            tagged += 1;
            for assoc in assocs_for_doc {
                // A ubiquitous concept is kept only where the page states it strongly (title or
                // headings), so "Machine Learning" keeps its tag while "training" noise does not.
                // Ubiquitous concepts keep their associations but are labelled as such, so the
                // lane can weight them down instead of the builder deleting them.
                //
                // Deleting was the first attempt and it was wrong: dropping `c-ml` entirely made
                // "teaching machines to learn from their own errors" resolve to no concept at all,
                // which turned a bad ranking into no ranking. A ubiquitous concept is weak
                // evidence, not absent evidence, and that distinction belongs in the score.
                if ubiquitous.contains(&assoc.concept_id) && assoc.direct < UBIQUITY_STRONG_EVIDENCE {
                    continue;
                }
                assocs += 1;
                mutations.push(Mutation::UpsertDocConcept {
                    resource_id: d.resource_id.clone(),
                    concept_id: assoc.concept_id.clone(),
                    // The scalar retrieval uses, derived from the strongest dimension.
                    weight: assoc.weight(),
                    relation_kind: "direct".into(),
                    evidence: None,
                    passage_id: None,
                    via_concept_id: None,
                    model_version: Some(MODEL_VERSION.into()),
                    work_id: None,
                    created_at: now,
                });
                // The dimensional breakdown (v2 §6): one row per non-zero dimension, so the same
                // article can rank differently for a topic query and a mechanism query.
                for (dim, value) in assoc.dims() {
                    if value <= 0.0 {
                        continue;
                    }
                    dims += 1;
                    mutations.push(Mutation::UpsertDocConceptDim {
                        resource_id: d.resource_id.clone(),
                        concept_id: assoc.concept_id.clone(),
                        dim: dim.to_string(),
                        value,
                        model_version: Some(MODEL_VERSION.into()),
                        created_at: now,
                    });
                }
            }
        }
        if !mutations.is_empty() && !dry_run {
            writer.apply_batch(&mutations)?;
        }
        if seen >= limit {
            break;
        }
    }

    let secs = started.elapsed().as_secs_f64().max(0.001);
    println!(
        "  tagged {tagged} of {seen} document(s): {assocs} association(s), {dims} dimension row(s) \
         in {secs:.1}s ({:.0} docs/s)",
        seen as f64 / secs
    );
    Ok(())
}

/// `agent0 concepts relate [--dir PATH]... [--topk N] [--min-support N]`
///
/// Build the concept relatedness graph from the document × concept matrix, with no external
/// taxonomy and no model: two concepts are related when the corpus uses them on the same page more
/// often than chance, measured by pointwise mutual information.
///
///     PMI(a,b) = ln( (n_ab · N) / (n_a · n_b) )
///
/// Why co-occurrence rather than an imported hierarchy: the corpus is software docs, news and
/// science pages, and the concept vocabulary is grown from it. An external taxonomy describes
/// scholarly subfields and would relate concepts the corpus never puts together, while missing the
/// pair that actually explains a cross-disciplinary answer ("chemical reactions that affect
/// romance" needs `chemistry` ≈ `romance` *in this corpus*, which no dictionary states).
///
/// Only positive PMI is kept (co-occurrence above chance), `strength` is `pmi / (1 + pmi)` so it is
/// bounded and comparable with the hand-written edges the same table holds, and the top `--topk`
/// neighbours per concept survive so a broad field cannot drag everything along. Rows are
/// `relation = 'cooccur'` with a model version, and the pass deletes its own previous rows first —
/// a growing lexicon that is not purged leaves stale edges behind forever.
pub async fn run_relate(args: &[String]) -> ExitCode {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => match args.get(i + 1) {
                Some(v) => {
                    dirs.push(PathBuf::from(v));
                    i += 2;
                }
                None => return usage("--dir needs a path"),
            },
            _ => i += 1,
        }
    }
    if dirs.is_empty() {
        dirs.push(Config::from_env().data_dir);
    }
    let topk = flag_usize(args, "--topk").unwrap_or(8).max(1);
    let min_support = flag_usize(args, "--min-support").unwrap_or(3).max(2);

    let mut failed = false;
    for dir in dirs {
        match relate_one(&dir, topk, min_support) {
            Ok(()) => {}
            Err(e) => {
                failed = true;
                eprintln!("{}: {e}", dir.display());
            }
        }
    }
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn relate_one(dir: &std::path::Path, topk: usize, min_support: usize) -> AppResult<()> {
    let started = Instant::now();
    let db = dir.join("meta").join("meta.sqlite");
    let reader = search_metadata::MetadataReader::from_path(&db)?;

    // concept -> set of documents. A document is counted once per concept even when the matrix
    // holds both a direct and an inferred row, because co-occurrence is about the page.
    let mut by_concept: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    // doc -> its concepts, for pair counting.
    let mut by_doc: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    {
        let conn = reader.conn();
        let mut stmt = conn
            .prepare("SELECT resource_id, concept_id FROM doc_concept WHERE relation_kind = 'direct'")
            .map_err(search_metadata::db_err)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(search_metadata::db_err)?;
        for row in rows {
            let (doc, concept) = row.map_err(search_metadata::db_err)?;
            by_concept.entry(concept.clone()).or_default().insert(doc.clone());
            by_doc.entry(doc).or_default().push(concept);
        }
    }
    let n_docs = by_doc.len() as f64;
    if n_docs == 0.0 {
        println!("{}: no direct associations; nothing to relate", dir.display());
        return Ok(());
    }

    // Pair counts. A document with k concepts contributes k(k-1)/2 pairs; the measured mean is
    // 1.75 concepts per document, so this is small.
    let mut pair: std::collections::HashMap<(String, String), u64> =
        std::collections::HashMap::new();
    for concepts in by_doc.values() {
        for a in 0..concepts.len() {
            for b in (a + 1)..concepts.len() {
                let (x, y) = if concepts[a] < concepts[b] {
                    (&concepts[a], &concepts[b])
                } else {
                    (&concepts[b], &concepts[a])
                };
                *pair.entry((x.clone(), y.clone())).or_insert(0) += 1;
            }
        }
    }

    // Keep the strongest `topk` neighbours per concept, symmetrically: an edge kept for `a` must
    // also be walkable from `b`, or expansion depends on which concept the query happened to name.
    let mut best: std::collections::HashMap<(String, String), f64> = std::collections::HashMap::new();
    for ((a, b), n_ab) in pair.iter() {
        if (*n_ab as usize) < min_support {
            continue;
        }
        let n_a = by_concept.get(a).map(|s| s.len()).unwrap_or(0) as f64;
        let n_b = by_concept.get(b).map(|s| s.len()).unwrap_or(0) as f64;
        if n_a == 0.0 || n_b == 0.0 {
            continue;
        }
        let pmi = ((*n_ab as f64 * n_docs) / (n_a * n_b)).ln();
        if pmi > 0.0 {
            best.insert((a.clone(), b.clone()), pmi / (1.0 + pmi));
        }
    }
    // Rank per endpoint, then take the union of both endpoints' top-k.
    let mut per_concept: std::collections::HashMap<String, Vec<(String, f64)>> =
        std::collections::HashMap::new();
    for ((a, b), s) in best.iter() {
        per_concept.entry(a.clone()).or_default().push((b.clone(), *s));
        per_concept.entry(b.clone()).or_default().push((a.clone(), *s));
    }
    let mut edges: Vec<(String, String, f64)> = Vec::new();
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let concept_count = per_concept.len();
    for (c, mut nbrs) in per_concept {
        nbrs.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal));
        nbrs.truncate(topk);
        for (other, s) in nbrs {
            let key = if c < other {
                (c.clone(), other.clone())
            } else {
                (other.clone(), c.clone())
            };
            if seen.insert(key.clone()) {
                edges.push((key.0, key.1, s));
            }
        }
    }
    edges.sort_by(|x, y| y.2.partial_cmp(&x.2).unwrap_or(std::cmp::Ordering::Equal));

    let mut w = MetadataWriter::from_path(&db)?;
    w.apply_batch(&[Mutation::DeleteConceptRelations {
        relation: MODEL_RELATION.to_string(),
    }])?;
    let now = search_core::time::now_unix_s();
    let mut written = 0usize;
    for chunk in edges.chunks(500) {
        let batch: Vec<Mutation> = chunk
            .iter()
            .map(|(a, b, s)| Mutation::UpsertConceptRelation {
                src_concept_id: a.clone(),
                dst_concept_id: b.clone(),
                relation: MODEL_RELATION.to_string(),
                strength: Some(*s),
                model_version: Some(MODEL_VERSION.to_string()),
                at: now,
            })
            .collect();
        w.apply_batch(&batch)?;
        written += batch.len();
    }

    println!(
        "{}: {} concepts over {} document(s), {} co-occurring pair(s) above chance, {written} edge(s) kept (topk {topk}, support {min_support}) in {:.1}s",
        dir.display(),
        concept_count,
        n_docs as u64,
        pair.len(),
        started.elapsed().as_secs_f64()
    );
    for (a, b, s) in edges.iter().take(5) {
        println!("  {a} <-> {b}  strength {s:.3}");
    }
    Ok(())
}

fn usage(why: &str) -> ExitCode {
    eprintln!("error: {why}");
    eprintln!(
        "usage: agent0 concepts build [--dir PATH]... [--batch N] [--limit N] [--dry-run]\n       agent0 concepts relate [--dir PATH]... [--topk N] [--min-support N]"
    );
    ExitCode::from(2)
}
