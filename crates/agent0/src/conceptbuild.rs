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

/// Stamped on every association and dimension row produced by this pass, so a later better
/// teacher can be introduced without guessing which rows it produced (v2 §4, §6).
const MODEL_VERSION: &str = "lexicon-v1";

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

/// `agent0 concepts build [--dir PATH]... [--batch N] [--limit N] [--dry-run]`
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
    let _ = flag_str(args, "--unknown");

    let mut failed = false;
    for dir in &dirs {
        let mut cfg = Config::from_env();
        cfg.data_dir = dir.clone();
        println!("{}:", dir.display());
        match build_root(&cfg, batch, limit, dry_run) {
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

fn build_root(cfg: &Config, batch: usize, limit: usize, dry_run: bool) -> AppResult<()> {
    let started = Instant::now();
    let now = search_core::time::now_unix_s();
    let mut writer = MetadataWriter::from_path(&cfg.meta_db())?;
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

fn usage(why: &str) -> ExitCode {
    eprintln!("error: {why}");
    eprintln!("usage: agent0 concepts build [--dir PATH]... [--batch N] [--limit N] [--dry-run]");
    ExitCode::from(2)
}
